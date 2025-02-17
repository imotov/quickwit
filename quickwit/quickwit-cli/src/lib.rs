// Copyright (C) 2023 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

#![deny(clippy::disallowed_methods)]

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{arg, Arg};
use dialoguer::theme::ColorfulTheme;
use dialoguer::Confirm;
use once_cell::sync::Lazy;
use quickwit_common::run_checklist;
use quickwit_common::runtimes::RuntimesConfiguration;
use quickwit_common::uri::Uri;
use quickwit_config::service::QuickwitService;
use quickwit_config::{ConfigFormat, QuickwitConfig, SourceConfig, DEFAULT_QW_CONFIG_PATH};
use quickwit_indexing::check_source_connectivity;
use quickwit_metastore::quickwit_metastore_uri_resolver;
use quickwit_storage::{load_file, quickwit_storage_uri_resolver};
use regex::Regex;
use tabled::object::Rows;
use tabled::{Alignment, Header, Modify, Style, Table, Tabled};
use tracing::info;

pub mod cli;
pub mod index;
#[cfg(feature = "jemalloc")]
pub mod jemalloc;
pub mod service;
pub mod source;
pub mod split;
pub mod stats;
pub mod tool;

/// Throughput calculation window size.
const THROUGHPUT_WINDOW_SIZE: usize = 5;

pub const QW_ENABLE_JAEGER_EXPORTER_ENV_KEY: &str = "QW_ENABLE_JAEGER_EXPORTER";

pub const QW_ENABLE_TOKIO_CONSOLE_ENV_KEY: &str = "QW_ENABLE_TOKIO_CONSOLE";

pub const QW_ENABLE_OPENTELEMETRY_OTLP_EXPORTER_ENV_KEY: &str =
    "QW_ENABLE_OPENTELEMETRY_OTLP_EXPORTER";

/// Regular expression representing a valid duration with unit.
pub const DURATION_WITH_UNIT_PATTERN: &str = r#"^(\d{1,3})(s|m|h|d)$"#;

fn config_cli_arg<'a>() -> Arg<'a> {
    Arg::new("config")
        .long("config")
        .help("Config file location")
        .env("QW_CONFIG")
        .default_value(DEFAULT_QW_CONFIG_PATH)
        .global(true)
        .display_order(1)
}

fn cluster_endpoint_arg<'a>() -> Arg<'a> {
    arg!(--"endpoint" <QW_CLUSTER_ENDPOINT> "Quickwit cluster endpoint.")
        .default_value("http://127.0.0.1:7280")
        .env("QW_CLUSTER_ENDPOINT")
        .required(false)
        .display_order(1)
        .global(true)
}

/// Parse duration with unit like `1s`, `2m`, `3h`, `5d`.
pub fn parse_duration_with_unit(duration_with_unit_str: &str) -> anyhow::Result<Duration> {
    static DURATION_WITH_UNIT_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(DURATION_WITH_UNIT_PATTERN).unwrap());
    let captures = DURATION_WITH_UNIT_RE
        .captures(duration_with_unit_str)
        .ok_or_else(|| anyhow::anyhow!("Invalid duration format: `[0-9]+[smhd]`"))?;
    let value = captures.get(1).unwrap().as_str().parse::<u64>().unwrap();
    let unit = captures.get(2).unwrap().as_str();

    match unit {
        "s" => Ok(Duration::from_secs(value)),
        "m" => Ok(Duration::from_secs(value * 60)),
        "h" => Ok(Duration::from_secs(value * 60 * 60)),
        "d" => Ok(Duration::from_secs(value * 60 * 60 * 24)),
        _ => bail!("Invalid duration format: `[0-9]+[smhd]`"),
    }
}

pub fn start_actor_runtimes(services: &HashSet<QuickwitService>) -> anyhow::Result<()> {
    if services.contains(&QuickwitService::Indexer)
        || services.contains(&QuickwitService::Janitor)
        || services.contains(&QuickwitService::ControlPlane)
    {
        let runtime_configuration = RuntimesConfiguration::default();
        quickwit_common::runtimes::initialize_runtimes(runtime_configuration)
            .context("Failed to start actor runtimes.")?;
    }
    Ok(())
}

async fn load_quickwit_config(config_uri: &Uri) -> anyhow::Result<QuickwitConfig> {
    let config_content = load_file(config_uri)
        .await
        .context("Failed to load quickwit config.")?;
    let config_format = ConfigFormat::sniff_from_uri(config_uri)?;
    let config = QuickwitConfig::load(config_format, config_content.as_slice())
        .await
        .with_context(|| format!("Failed to deserialize quickwit config `{config_uri}`."))?;
    info!(config_uri=%config_uri, config=?config, "Loaded Quickwit config.");
    Ok(config)
}

/// Runs connectivity checks for a given `metastore_uri` and `index_id`.
/// Optionally, it takes a `SourceConfig` that will be checked instead
/// of the index's sources.
pub async fn run_index_checklist(
    metastore_uri: &Uri,
    index_id: &str,
    source_to_check: Option<&SourceConfig>,
) -> anyhow::Result<()> {
    let mut checks: Vec<(&str, anyhow::Result<()>)> = Vec::new();
    let metastore_uri_resolver = quickwit_metastore_uri_resolver();
    let metastore = metastore_uri_resolver.resolve(metastore_uri).await?;
    checks.push(("metastore", metastore.check_connectivity().await));

    let index_metadata = metastore.index_metadata(index_id).await?;
    let storage_uri_resolver = quickwit_storage_uri_resolver();
    let storage = storage_uri_resolver.resolve(index_metadata.index_uri())?;
    checks.push(("storage", storage.check_connectivity().await));

    if let Some(source_config) = source_to_check {
        checks.push((
            source_config.source_id.as_str(),
            check_source_connectivity(source_config).await,
        ));
    } else {
        for source_config in index_metadata.sources.values() {
            checks.push((
                source_config.source_id.as_str(),
                check_source_connectivity(source_config).await,
            ));
        }
    }
    run_checklist(checks)?;
    Ok(())
}

/// Constructs a table for display.
pub fn make_table<T: Tabled>(
    header: &str,
    rows: impl IntoIterator<Item = T>,
    transpose: bool,
) -> Table {
    let table = if transpose {
        let mut index_builder = Table::builder(rows).index();
        index_builder.set_index(0);
        index_builder.transpose();
        index_builder.build()
    } else {
        Table::builder(rows).build()
    };

    table
        .with(Modify::new(Rows::new(1..)).with(Alignment::left()))
        .with(Style::ascii())
        .with(Header(header))
        .with(Modify::new(Rows::single(0)).with(Alignment::center()))
}

/// Prompts user for confirmation.
fn prompt_confirmation(prompt: &str, default: bool) -> bool {
    if Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .default(default)
        .interact()
        .unwrap()
    {
        true
    } else {
        println!("Aborting.");
        false
    }
}

pub mod busy_detector {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Instant;

    use once_cell::sync::Lazy;
    use tracing::warn;

    // we need that time reference to use an atomic and not a mutex for LAST_UNPARK
    static TIME_REF: Lazy<Instant> = Lazy::new(Instant::now);
    static ENABLED: AtomicBool = AtomicBool::new(false);

    const ALLOWED_DELAY_MICROS: u64 = 5000;
    const WARN_SUPPRESSION_MICROS: u64 = 30_000_000;

    // LAST_UNPARK_TIMESTAMP and NEXT_WARN_TIMESTAMP are semantically micro-second
    // precision timestamps, but we use atomics to allow accessing them without locks.
    thread_local!(static LAST_UNPARK_TIMESTAMP: AtomicU64 = AtomicU64::new(0));
    static NEXT_WARN_TIMESTAMP: AtomicU64 = AtomicU64::new(0);
    static SUPPRESSED_WARN_COUNT: AtomicU64 = AtomicU64::new(0);

    pub fn set_enabled(enabled: bool) {
        ENABLED.store(enabled, Ordering::Relaxed);
    }

    pub fn thread_unpark() {
        LAST_UNPARK_TIMESTAMP.with(|time| {
            let now = Instant::now()
                .checked_duration_since(*TIME_REF)
                .unwrap_or_default();
            time.store(now.as_micros() as u64, Ordering::Relaxed);
        })
    }

    pub fn thread_park() {
        if !ENABLED.load(Ordering::Relaxed) {
            return;
        }

        LAST_UNPARK_TIMESTAMP.with(|time| {
            let now = Instant::now()
                .checked_duration_since(*TIME_REF)
                .unwrap_or_default();
            let now = now.as_micros() as u64;
            let delta = now - time.load(Ordering::Relaxed);
            if delta > ALLOWED_DELAY_MICROS {
                emit_warn(delta, now);
            }
        })
    }

    fn emit_warn(delta: u64, now: u64) {
        if NEXT_WARN_TIMESTAMP
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next_warn| {
                if next_warn < now {
                    Some(now + WARN_SUPPRESSION_MICROS)
                } else {
                    None
                }
            })
            .is_err()
        {
            // a warn was emited recently, don't emit log for this one
            SUPPRESSED_WARN_COUNT.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let suppressed = SUPPRESSED_WARN_COUNT.swap(0, Ordering::Relaxed);
        if suppressed == 0 {
            warn!("Thread wasn't parked for {delta}µs, is the runtime too busy?");
        } else {
            warn!(
                "Thread wasn't parked for {delta}µs, is the runtime too busy? ({suppressed} \
                 similar messages suppressed)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::parse_duration_with_unit;

    #[test]
    fn test_parse_duration_with_unit() -> anyhow::Result<()> {
        assert_eq!(parse_duration_with_unit("8s")?, Duration::from_secs(8));
        assert_eq!(parse_duration_with_unit("5m")?, Duration::from_secs(5 * 60));
        assert_eq!(
            parse_duration_with_unit("2h")?,
            Duration::from_secs(2 * 60 * 60)
        );
        assert_eq!(
            parse_duration_with_unit("3d")?,
            Duration::from_secs(3 * 60 * 60 * 24)
        );

        assert!(parse_duration_with_unit("").is_err());
        assert!(parse_duration_with_unit("a2d").is_err());
        assert!(parse_duration_with_unit("3 d").is_err());
        assert!(parse_duration_with_unit("3").is_err());
        assert!(parse_duration_with_unit("1h30").is_err());
        Ok(())
    }
}
