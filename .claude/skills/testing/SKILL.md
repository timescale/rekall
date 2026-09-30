---
name: testing
description: >
  Which of this repo's test kinds to use for a change, where new tests
  belong, what needs a live PostgreSQL, and the gotchas that produce
  false-positive passes. Trigger when writing tests for new or changed
  code, or judging whether coverage of a change is adequate.
---

# Testing

Tests exist to safeguard the correctness of the code — not the other way
around. There is no goal of "making the tests pass" for its own sake. If a
test fails, the default assumption is that the code is wrong; only change
the test itself when you've independently confirmed the test's expectation
was wrong, not merely inconvenient. Never edit, weaken, delete, or skip a
test in order to get it to pass without first understanding *why* it
failed — a green suite obtained that way is worse than a red one, because
it actively hides the problem instead of reporting it.

For a benchmark tool the bar is higher than "doesn't crash": a wrong
recall, percentile or QPS figure is published and compared against other
systems. The metric math, the config parsing that decides what runs, and
the phases that decide what is measured deserve tests with independently
derived expected values.

Three kinds of test exist here:

- **Unit tests** — `#[cfg(test)] mod tests` next to the code, run with
  `cargo test -p rekall-lib --lib`. No PostgreSQL, no network, no dataset
  files beyond what the test writes itself. This is what the pre-commit
  hook runs on every commit, so it's the fastest feedback loop — prefer
  these for metrics, config, encoding, planning and report logic.
- **Integration tests** — `rekall-lib/tests/pg_integration.rs`, and the
  few `#[ignore]`d tests inside the crate. They need a live PostgreSQL
  with pgvector (and pg_vectorsearch unless
  `REKALL_TEST_PG_VECTORSEARCH=0`), reached through `REKALL_TEST_DSN`:

  ```bash
  REKALL_TEST_DSN="host=localhost dbname=rekall_test" \
      cargo test --test pg_integration
  REKALL_TEST_DSN=... cargo test -p rekall-lib --lib -- --ignored
  ```

  Without the DSN they return early and count as passed. CI has no
  database, so **a green run proves nothing about them**: run them
  yourself when touching backends, the loader, index builds or the
  search phase, and say so in the PR.
- **Dry runs** — `rekall run <config.toml> --dry-run` prints what a config
  would execute. Cheap end-to-end check that a config change or a new
  option is wired all the way through the CLI.

Pick the cheapest kind that actually exercises the change — don't reach
for a live-database test where a unit test of the SQL builder or the
metric would do.

## Where new tests go

Put a unit test in the `tests` module of the file it exercises; the module
already exists in most files. Don't proliferate integration test files:
`pg_integration.rs` is organized by section, and a new backend or phase
check belongs in the matching section. When appending, minimize the diff
rather than reorganizing what's already there.

## Gotcha: unit tests run on any machine

The unit suite runs on developer laptops, EC2 boxes and CI runners with
whatever disks, devices and environment they happen to have. A test that
reads the machine is a test of that machine:

- **Don't read `/sys`, `/proc`, environment variables or the EC2 metadata
  service from a unit test.** Code that inspects the host (`environment.rs`
  and its storage detection) should split into a pure function over the
  inputs and a thin reader; the test covers the pure function with its
  own inputs.
- **Tests run in parallel threads.** Anything process-global — the current
  directory, environment variables, a fixed port or path — races with the
  other tests. Use `tempfile::tempdir()` for files and pass values in
  rather than setting `std::env`.
- **Close what you open.** HDF5 holds an exclusive lock on a file open for
  writing, so a test that writes a file and reads it back must close the
  writer first; drop order alone is not a guarantee. Create test files
  with the strong close degree and call `close()` before handing the path
  on.
- **No timing assertions** beyond "finished at all". QPS and latency are
  measured by the tool, not by its tests.

## Writing tests that actually test something

- Derive expected values independently — a recall computed by hand on a
  five-row ground truth, a percentile of a known distribution, the exact
  SQL a backend should emit — not by copying whatever the code printed.
  A test whose expectation was pasted from the output proves the code
  matches itself.
- Cover the edge cases identified during planning, not just the happy
  path: an empty result set, a filter that removes everything, a dataset
  smaller than the sample, a config that sets an alias and the canonical
  key at once.
- A test that would still pass if the logic under test were deleted or
  wrong isn't testing anything — assert on the specific behavior, not
  just "it didn't error" or "the vector has the right length".
- Error paths are behavior too: a bad config must fail with the message a
  user can act on, and the test should check the message names the key.
- A feature isn't done when it compiles and passes the happy path; it's
  done when its edge cases and failure modes have tests.
