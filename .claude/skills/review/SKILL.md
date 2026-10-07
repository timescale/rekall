---
name: review
description: >
  This repo's review checklist — reviewing a plan before implementation,
  and adversarially reviewing a finished branch's diff before opening a
  PR. Used by `build-feature`. Trigger when asked to review a plan, or to
  adversarially review a branch/diff for correctness, measurement
  fidelity, scalability, safety, cleanliness, or documentation drift.
---

# Review

Two kinds of artifact get reviewed in this repo's feature workflow (see
`build-feature`): a plan, before any code is written, and a branch's diff,
once the feature is finished or nearly so. The same priorities apply to
both, aimed at different material — a plan describes an intended design,
a diff is the actual result.

Both should be reviewed by a fresh agent — not the one that produced the
artifact being reviewed — so the review isn't reasoning from the same
assumptions the author had.

## Priority order

Weigh findings in this order, whether reviewing a plan or a diff:

1. **Measurement fidelity** — rekall's output is compared across index
   types and published. Anything that changes what gets timed, what
   counts as a hit, how percentiles are computed, or how much the tool
   itself perturbs the database under test outranks everything else.
2. **Scalability** — the tool must run against 100M+ vectors on a machine
   whose memory does not scale with the dataset. Memory allocations
   **MUST** be bounded: stream, batch, and keep bounded channels between
   the reader and the writer; never collect a dataset, a result set, or a
   run's history into memory in proportion to its size.
3. **Safety of what the tool does to the world** — it drops and creates
   tables and indexes, restarts PostgreSQL, launches and terminates cloud
   instances, and publishes results. Destructive actions must be scoped
   to what the config names, confirmed where the CLI already asks, and
   never widened by a refactor. Credentials and DSNs stay out of logs,
   result files and published pages.
4. **Simplicity, architecture, and clean code** — once the above are
   satisfied. This includes avoiding duplication: check whether existing
   code already does the same thing — directly callable, or after a
   small refactor — before accepting new logic that reimplements it. This
   is a good use of a fresh reviewer, since the author may not have
   looked as hard for something to reuse as someone starting cold.

## Reviewing a plan

Check the plan against the priority order above, plus general soundness:

- **Measurement implications** — does the design add work inside a timed
  region, change warm-up, sampling or query ordering, or poll the server
  in a way that competes with the queries being measured?
- **Scalability** — where does memory go as the dataset grows? A plan
  that holds vectors, IDs or results in a `Vec` sized by the dataset
  needs a streaming or batched alternative, or an explicit bound.
- **Safety** — which tables, indexes, clusters or instances does the
  plan touch, and how does it make sure they are the ones the user meant?
- **Simplicity and architecture** — is this the simplest design that
  still satisfies the priorities above? Check it against the actual code
  and CLAUDE.md, not just the plan's own description of the existing
  patterns (the `IndexBackend` trait and its opaque params, the phase
  split, the output sinks). Flag a simpler alternative the plan passed
  over without justification, and specifically flag duplication.
- Missing edge cases or unstated assumptions: an empty table, a dataset
  without a `test` split, a backend that isn't installed, a resumed run.
- Steps underspecified enough to invite improvisation mid-implementation.

Bring findings back to the user; revise and re-review if there are
material concerns before implementing.

## Reviewing code (adversarial)

Review the diff against the plan and this codebase's conventions, actively
trying to find something wrong rather than confirming it looks fine.

### Correctness and tests

- Correctness bugs, not just style nits.
- Gaps between what the plan promised and what actually got built.
- Missing test coverage: edge cases without a test, or the wrong kind of
  test for what's being checked (see the `testing` skill). Integration
  tests that need a database silently pass without one — ask whether they
  were actually run.
- **Tests shaped to pass regardless of whether the code is actually
  correct.** This is the specific failure mode of an implementer (agent or
  not) writing the test *after* the code and unconsciously fitting the
  test to whatever the code already does, rather than to what it should
  do. For every test touching the new logic, check whether the expected
  value was derived independently — from the plan, the spec, or a
  known-correct calculation — or whether it was just copied from whatever
  the implementation happened to output. The latter proves nothing.
  Concretely: read the assertion and ask whether it would still pass if
  the specific bug the plan was written to avoid were reintroduced; if it
  would, the test isn't testing that bug. Also watch for the softer
  versions of this — overly loose tolerances, swallowed errors,
  assertions weakened until they pass, or a test disabled/skipped rather
  than fixed.
- Tests that read the machine or the environment (see the `testing`
  skill's gotchas) — they pass here and fail on the next runner.
- Anything that would fail the checks in `create-pr` (`cargo fmt`,
  `clippy -D warnings`, `cargo test --workspace --locked`) before those
  are even run.

### Code cleanliness and consistency

A review focused on correctness tends to skim past this, so call it out
explicitly as its own concern:

- **Duplication.** Is any of the new code a near-copy of something that
  already exists in `rekall-lib/`? Prefer pointing at the existing code to
  reuse (or extract into a shared helper) over new code that happens to
  do the same thing slightly differently. Backends in particular share
  the PostgreSQL connection, encoding and monitoring layers — a new
  backend should not grow its own copies.
- **Error handling.** Library code returns the crate's `Error` (or a
  typed error that converts into it) with the context a user needs to
  act — the path, the key, the table — and the CLI turns it into a
  message through `anyhow`. A `panic!`, `unwrap()` or `expect()` on a
  path that user input or the database can reach is a bug; they belong
  to invariants the code itself guarantees.
- **Async discipline.** Blocking work (HDF5 reads, file IO, hashing)
  runs on a blocking thread and feeds the async side through a bounded
  channel, as the loader does. A blocking call inside an async function
  stalls the runtime and skews every measurement in flight.
- **Naming consistency.** Do new types, functions, config keys and CLI
  flags match this codebase's existing conventions — checked against
  neighboring code, not just consistency within the new diff?
- **Public surface especially.** New config keys, CLI flags and result
  file fields deserve more scrutiny than internal code: they are read by
  configs under `benchmarks/`, by published result files, and by the
  comparison tooling. A config key is part of every TOML anyone has
  written; result fields are read back from files older than the code,
  so new ones need `serde(default)` and a story for their absence.

### Documentation consistency

Check whether this repo's docs still match what the branch actually
implements — not just whether the feature's own new docs were written,
but whether any *existing* doc describing the touched area is now stale.
`README.md`, `docs/configuration.md` (every config key and
default) and `docs/cloud-infrastructure.md` (the `infra` commands and
provisioning) are where to look, and CLAUDE.md's own "Documentation
Maintenance" section names them — but the bar is accuracy, not presence:
a stale description is worse than no description, since it actively
misleads the next reader instead of just leaving a gap. Flag any doc
whose text still describes the old behavior, a changed default, a removed
key, or a number (a limit, a sample size, a timeout) that the code no
longer matches — not only missing docs for what's new.

### Safety

If the feature touches anything that drops or creates database objects,
restarts or reconfigures PostgreSQL, launches, resizes or terminates
cloud instances, or publishes results, run a dedicated pass with the
third priority in mind — a reviewer told to look for correctness bugs
tends not to also be thinking about what a wrong table name would delete.
Check specifically for:

- Table, index and schema names built by string formatting from config
  values — are they quoted, and can a config make the statement reach an
  object the run does not own?
- Operations whose scope widened: a `DROP ... CASCADE` where a plain drop
  was, a cleanup that now matches a prefix instead of a name, an instance
  filter that lost a tag.
- DSNs, tokens or AWS identifiers in log lines, result files, or anything
  under `results/` that is published.
- Resumption and idempotence: rerunning a phase after a crash must not
  double-load, double-count, or leave a half-built index the next phase
  measures.

### Measurement and scalability

If the feature touches the load, index or search phases, or the monitors
that watch them, review with measurement in mind as well as correctness.
Flag anything that adds work inside a timed region (an allocation per
query, a log line per row, a metadata query on the hot path), changes
when a timer starts or stops, or polls the server often enough to compete
with the benchmark. Code inspection alone can miss a regression a run
would catch immediately: when the change touches a phase, run the same
small config before and after and compare the reported figures, not just
whether it completed.

Take extra note of memory usage patterns. There **MUST** be no memory
allocations that pile up in proportion to the size of the dataset or the
work at hand — the loader streams chunks through a bounded channel for
exactly this reason, and the search phase bounds the result history it
keeps. A feature that needs to see all rows should iterate in batches
and release each batch before the next; if it cannot, that is a design
finding for the user, not something to accept because it works on a
1M-row dataset.

### Reporting findings

Report findings to the user. Treat weak/missing/self-serving test
coverage, measurement and safety findings at least as seriously as a
correctness bug — fix what's confirmed real rather than deferring it to a
follow-up, and don't paper over a finding just to get to the PR. If
something is a deliberate trade-off rather than a bug, say so instead of
silently dismissing it.
