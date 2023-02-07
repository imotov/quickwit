// Copyright (C) 2022 Quickwit, Inc.
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

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::future;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::command::Observe;
use crate::mailbox::WeakMailbox;
use crate::{Actor, ActorExitStatus, Command, Mailbox};

struct TypedJsonObservable<A: Actor> {
    actor_instance_id: String,
    weak_mailbox: WeakMailbox<A>,
    join_handle: ActorJoinHandle,
}

#[async_trait]
trait JsonObservable: Sync + Send {
    fn is_disconnected(&self) -> bool;
    fn any(&self) -> &dyn Any;
    fn actor_instance_id(&self) -> &str;
    async fn observe(&self) -> Option<JsonValue>;
    async fn quit(&self) -> ActorExitStatus;
    async fn join(&self) -> ActorExitStatus;
}

#[async_trait]
impl<A: Actor> JsonObservable for TypedJsonObservable<A> {
    fn is_disconnected(&self) -> bool {
        self.weak_mailbox
            .upgrade()
            .map(|mailbox| mailbox.is_disconnected())
            .unwrap_or(true)
    }
    fn any(&self) -> &dyn Any {
        &self.weak_mailbox
    }
    fn actor_instance_id(&self) -> &str {
        self.actor_instance_id.as_str()
    }
    async fn observe(&self) -> Option<JsonValue> {
        let mailbox = self.weak_mailbox.upgrade()?;
        let oneshot_rx = mailbox.send_message_with_high_priority(Observe).ok()?;
        let state: <A as Actor>::ObservableState = oneshot_rx.await.ok()?;
        serde_json::to_value(&state).ok()
    }

    async fn quit(&self) -> ActorExitStatus {
        let _ = self
            .weak_mailbox
            .upgrade()
            .map(|mailbox| {
                let _ = mailbox.send_message_with_high_priority(Command::Quit);
            })
            .unwrap();
        self.join().await
    }

    async fn join(&self) -> ActorExitStatus {
        self.join_handle.join().await
    }
}

#[derive(Default, Clone)]
pub(crate) struct ActorRegistry {
    actors: Arc<RwLock<HashMap<TypeId, ActorRegistryForSpecificType>>>,
}

struct ActorRegistryForSpecificType {
    type_name: &'static str,
    observables: Vec<Arc<dyn JsonObservable>>,
}

impl ActorRegistryForSpecificType {
    fn for_type<A>() -> ActorRegistryForSpecificType {
        ActorRegistryForSpecificType {
            type_name: std::any::type_name::<A>(),
            observables: Vec::new(),
        }
    }

    fn gc(&mut self) {
        let mut i = 0;
        while i < self.observables.len() {
            if self.observables[i].is_disconnected() {
                self.observables.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}

#[derive(Serialize, Debug)]
pub struct ActorObservation {
    pub type_name: &'static str,
    pub instance_id: String,
    pub obs: Option<JsonValue>,
}

impl ActorRegistry {
    pub fn register<A: Actor>(&self, mailbox: &Mailbox<A>, join_handle: ActorJoinHandle) {
        let typed_id = TypeId::of::<A>();
        let actor_instance_id = mailbox.actor_instance_id().to_string();
        let weak_mailbox = mailbox.downgrade();
        self.actors
            .write()
            .unwrap()
            .entry(typed_id)
            .or_insert_with(|| ActorRegistryForSpecificType::for_type::<A>())
            .observables
            .push(Arc::new(TypedJsonObservable {
                weak_mailbox,
                actor_instance_id,
                join_handle: join_handle,
            }));
    }

    pub async fn observe(&self, timeout: Duration) -> Vec<ActorObservation> {
        self.gc();
        let mut obs_futures = Vec::new();
        for registry_for_type in self.actors.read().unwrap().values() {
            for obs in &registry_for_type.observables {
                if obs.is_disconnected() {
                    continue;
                }
                let obs_clone = obs.clone();
                let type_name = registry_for_type.type_name;
                let instance_id = obs.actor_instance_id().to_string();
                obs_futures.push(async move {
                    let obs = tokio::time::timeout(timeout, obs_clone.observe())
                        .await
                        .unwrap_or(None);
                    ActorObservation {
                        type_name,
                        instance_id,
                        obs,
                    }
                });
            }
        }
        future::join_all(obs_futures.into_iter()).await
    }

    pub fn get<A: Actor>(&self) -> Vec<Mailbox<A>> {
        let mut lock = self.actors.write().unwrap();
        get_iter::<A>(&mut lock).collect()
    }

    pub fn get_one<A: Actor>(&self) -> Option<Mailbox<A>> {
        let mut lock = self.actors.write().unwrap();
        let opt = get_iter::<A>(&mut lock).next();
        opt
    }

    fn gc(&self) {
        for registry_for_type in self.actors.write().unwrap().values_mut() {
            registry_for_type.gc();
        }
    }

    pub async fn quit(&self) -> Vec<ActorExitStatus> {
        let mut obs_futures = Vec::new();
        for registry_for_type in self.actors.read().unwrap().values() {
            for obs in &registry_for_type.observables {
                if obs.is_disconnected() {
                    continue;
                }
                let obs_clone = obs.clone();
                obs_futures.push(async move { obs_clone.quit().await });
            }
        }
        let res = future::join_all(obs_futures).await;
        let statuses = res.into_iter().map(|s| s.clone()).collect();
        statuses
    }

    pub async fn join(&self) -> Vec<ActorExitStatus> {
        let mut obs_futures = Vec::new();
        for registry_for_type in self.actors.read().unwrap().values() {
            for obs in &registry_for_type.observables {
                if obs.is_disconnected() {
                    continue;
                }
                let obs_clone = obs.clone();
                obs_futures.push(async move { obs_clone.join().await });
            }
        }
        let res = future::join_all(obs_futures).await;
        let statuses = res.into_iter().map(|s| s.clone()).collect();
        statuses
    }
}

fn get_iter<A: Actor>(
    actors: &mut HashMap<TypeId, ActorRegistryForSpecificType>,
) -> impl Iterator<Item = Mailbox<A>> + '_ {
    let typed_id = TypeId::of::<A>();
    actors
        .get(&typed_id)
        .into_iter()
        .flat_map(|registry_for_type| {
            registry_for_type
                .observables
                .iter()
                .flat_map(|box_any| box_any.any().downcast_ref::<WeakMailbox<A>>())
                .flat_map(|weak_mailbox| weak_mailbox.upgrade())
        })
        .filter(|mailbox| !mailbox.is_disconnected())
}

enum ActorJoinHandleState {
    Active(JoinHandle<ActorExitStatus>),
    Exited(ActorExitStatus),
}

impl Default for ActorJoinHandleState {
    fn default() -> Self {
        Self::Exited(ActorExitStatus::Panicked)
    }
}

/// This structure contains either a handle or exit state if the handle was already used.
/// This allows us to perform a join from both - universe and actor handle
#[derive(Clone)]
pub(crate) struct ActorJoinHandle {
    holder: Arc<Mutex<ActorJoinHandleState>>,
}

impl ActorJoinHandle {
    pub(crate) fn new(join_handle: JoinHandle<ActorExitStatus>) -> Self {
        ActorJoinHandle {
            holder: Arc::new(Mutex::new(ActorJoinHandleState::Active(join_handle))),
        }
    }

    pub(crate) async fn join(&self) -> ActorExitStatus {
        let mut guard = self.holder.lock().await;
        match &*guard {
            ActorJoinHandleState::Active(_) => {
                // set a temporary placeholder as panicked
                let handle = mem::take(&mut *guard);
                match handle {
                    ActorJoinHandleState::Active(join_handle) => {
                        let exit_status = join_handle.await.unwrap_or_else(|join_err| {
                            if join_err.is_panic() {
                                return ActorExitStatus::Panicked;
                            } else {
                                return ActorExitStatus::Killed;
                            }
                        });
                        // replace it with real value
                        *guard = ActorJoinHandleState::Exited(exit_status.clone());
                        exit_status
                    }
                    ActorJoinHandleState::Exited(state) => state, // just to make compiler happy - we shouldn't be here because of mutex
                }
            }
            ActorJoinHandleState::Exited(status) => /*status.clone()*/ panic!("Shouldn't join"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::tests::PingReceiverActor;
    use crate::Universe;

    #[tokio::test]
    async fn test_registry() {
        let test_actor = PingReceiverActor::default();
        let universe = Universe::with_accelerated_time();
        let (_mailbox, _handle) = universe.spawn_builder().spawn(test_actor);
        let _actor_mailbox = universe.get_one::<PingReceiverActor>().unwrap();
    }

    #[tokio::test]
    async fn test_registry_killed_actor() {
        let test_actor = PingReceiverActor::default();
        let universe = Universe::with_accelerated_time();
        let (_mailbox, handle) = universe.spawn_builder().spawn(test_actor);
        handle.kill().await;
        assert!(universe.get_one::<PingReceiverActor>().is_none());
    }

    #[tokio::test]
    async fn test_registry_last_mailbox_dropped_actor() {
        let test_actor = PingReceiverActor::default();
        let universe = Universe::with_accelerated_time();
        let (mailbox, handle) = universe.spawn_builder().spawn(test_actor);
        drop(mailbox);
        handle.join().await;
        assert!(universe.get_one::<PingReceiverActor>().is_none());
    }

    #[tokio::test]
    async fn test_get_actor_states() {
        let test_actor = PingReceiverActor::default();
        let universe = Universe::with_accelerated_time();
        let (_mailbox, _handle) = universe.spawn_builder().spawn(test_actor);
        let obs = universe.observe(Duration::from_millis(1000)).await;
        assert_eq!(obs.len(), 1);
    }
}
