# Rest API tests

This directory is meant to test quickwit at the Rest API level.
It was originally meant to iterate over the elastic search compatibility API,
but can also be used as a convenient way to create integration tests.

# Running the tests

The test script is meant to target `elasticsearch` and `quickwit`.

When targetting quickwit, the script expects a fresh quickwit instance
to be running on `http://localhost:7280`. The data involved is small and
running in DEBUG mode is fine.

```./rest_api_test.py --engine quickwit```

When targetting elasticsearch, the script expects elastic to be running on
`http://localhost:9200`.

In both case, the test will take care of setting up, ingesting and tearing down the
indexes involved.

```./rest_api_test.py --engine elasticsearch```

# Writing new tests

Writing a new test suite only requires to create a new subdirectory somewhere in the scenarii.
The test script recursively browse the directories and execute the `setup.{engine}.sh` and
`teardow.{engine}.sh` scripts as it enter and exits directories.

It then executes the tests described in .yaml files, in their lexicographical order.
A single file can contain more than one tests, by separating them by `---`.

Here is an example of a test

```yaml
# Query string takes priority over query defined in body
method: [GET, POST]
params:
  # this overrides the query sent in body
  q: type:PushEvent
  size: 3
json:
  query:
    term:
      type:
        value: "whatever"
expected:
  hits:
    total:
      value: 60
      relation: "eq"
    hits:
      $expect: "len(val) == 3"
```

A test will just run a REST HTTP call, and check that the resulting JSON matches
some expectation.


- **method**: gives the list of HTTP methods to test. If there is more than one, they will be all tested.
- **params**: describes the parameters that should be sent as query strings.
- **json**: describes the JSON body, sent with the query
- **expected**: describes the expectation.

# Expectations

The expectation is an object that mirrors the structure of the response.
It does not need to contain its entire tree.

For instance, given the following json object:
```json
{"name": "Droopy", "age": 31}
```

It is possible to test for the name part only by using the following expectation:
```yaml
# ...
expected:
  name: Droopy
```

Sometimes, it might be cumbersome or even impossible to check a result against a value.
In that case, it is possible to express the condition as a python expression, by using the reserved keyword "$expect".

In the following, we could check that the age is greater than 30, like this:
```yaml
# ...
expected:
  age:
      $expect: "val >= 3"
```

Note that the value of the node (here `31`) is injected as a variable `val` in the expression.
