# CLAUDE.md

## Project Overview

Rekall is a Rust-based vector search benchmark tool. It measures recall-vs-QPS
tradeoffs for PostgreSQL vector indexes (Meerkat, pgvector HNSW, VectorChord)
with streaming data loading that scales to 100M+ vectors.

## Build

```bash
cargo build                    # Debug build
cargo build --release          # Release build
```

## Usage

```bash
rekall dataset info <file.hdf5>    # Inspect dataset
rekall run <config.toml>           # Full benchmark
rekall run <config.toml> --dry-run # Show what would run
rekall init <config.toml>          # Init DB only
rekall load <config.toml>          # Load data only
rekall index <config.toml>         # Build indexes only
rekall search <config.toml>        # Search only
```

## Architecture

- `rekall-lib/` — Core library
  - `backend/` — IndexBackend trait + PG implementations (meerkat, pgvector)
  - `dataset/` — Streaming HDF5 reader
  - `phase/` — init, load, index, search, report orchestration
  - `output/` — Pluggable sinks (stdout, JSON)
  - `config.rs` — TOML config parsing
  - `metrics.rs` — Recall, QPS, latency percentiles, Pareto frontier
- `rekall/` — CLI binary (clap) + Agent orchestrator

## Key Patterns

- **Streaming COPY BINARY**: HDF5 chunks read in a blocking thread, sent via
  channel to async COPY writer. Bounded memory regardless of dataset size.
- **Shared table**: Multiple index backends share the same loaded table.
- **Opaque params**: Each IndexBackend parses its own `toml::Value` params.

## Development Approach

- **Step-by-step with review**: work in small increments and pause for review
  after each step. Don't implement multiple components at once unless
  explicitly asked to do larger work unattended.
- Well-tested changes: unit tests next to the code, integration tests against
  a live PostgreSQL for the backends and phases (see the `testing` skill).
- **CI scripts**: put larger CI workflow jobs in standalone bash scripts in
  `scripts/ci/`. Scripts should be runnable locally for testing and
  debugging. GitHub Actions workflows should call these scripts rather than
  inline commands.

## Code Style

`cargo fmt` and `cargo clippy --all-targets -- -D warnings` are the style
guide; the pre-commit hooks and CI run both, so a change that passes them
is formatted and lint-clean by definition. Beyond that:

- Prefer self-documenting code using aptly-named functions and variables
  over expansive comments.
- Library code returns the crate's `Error` with the context a user needs to
  act on (the path, the config key, the table); the CLI turns it into a
  message through `anyhow`. `unwrap()`, `expect()` and `panic!` are for
  invariants the code itself guarantees, never for paths that user input or
  the database can reach.
- Blocking work (HDF5 reads, file IO) runs on a blocking thread and feeds
  the async side through a bounded channel, as the loader does.
- Memory must stay bounded as the dataset grows: stream and batch rather
  than collect. A `Vec` sized by rows, queries or results is a bug on a
  100M-vector run.

### Code comments

Keep comments short, lean, and to the point. First describe in one sentence
what a function, type or field is for, then focus on describing any
non-obvious behavior. Write for humans. Avoid a lot of ';' and ':'-style
writing, and generally obtuse language.

Doc comments (`///`) go on public items and on anything whose purpose is
not obvious from its name; `//!` at the top of a module says what the
module is for. Inside a function, a comment explains a decision the code
cannot express, not what the next line does.

A self-check is to see if the ratio between code and comments in a change
leans towards comments. Err on the side of too short rather than too long
comments.

Never explain what something did in the past or before a change (unless
motivated by a serious bug that we should avoid regressing back to). Never
explain the history of changes. Focus on what the code does NOW, not what
it did in the past. Readers care about understanding the code as it is.

## Documentation Maintenance

Keep documentation up-to-date with code changes:

- `README.md`: features, quick start, and the commands a user runs.
- `docs/configuration.md`: every config key and its default. A new or
  changed key is not done until it is documented here.
- `docs/cloud-infrastructure.md`: the `infra` commands and provisioning.

## GitHub Workflows

Use [zizmor](https://docs.zizmor.sh/) to check workflow files for security
best practices:

```bash
zizmor --pedantic .github/workflows/
```

Workflow requirements:

- Pin actions to commit hashes (not tags)
- Add `persist-credentials: false` to checkouts
- Add explicit `permissions` blocks
- Add `concurrency` settings to cancel duplicate runs

## Git Rules

- **Never use `git add -A`, `git add .`, or `git commit -a`**. Always stage
  files explicitly by name to avoid committing binaries, results, or local
  configs.
- Review staged changes with `git diff --staged` before committing.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat(search): ...`, `fix: ...`, `bench: ...`, `infra: ...`). A commit-msg
  hook checks this locally once `pre-commit install` has run, and CI checks
  every commit of a pull request.
- The `.gitignore` excludes `/target`, `results/`, `*.local.toml`, `/data/`,
  `/configs/`, `plot.*` — but explicit staging is still required as a safeguard.

## Dependencies

Requires system libhdf5 (uses `hdf5-metno` crate for HDF5 2.x support).
PostgreSQL with pgvector extension for vector column type.

## Development Workflow

Work follows a branch and pull request workflow — never commit directly to
`main`. The full checklist (branch naming, hooks, commit message format,
rebase/push, PR body style, post-merge cleanup) lives in Claude Code skills
under `.claude/skills/` rather than here:

- `git-workflow` — starting a branch, working in reviewable increments,
  addressing review feedback, cleaning up after merge.
- `create-pr` — pre-commit checks, committing, rebasing, pushing, and
  opening the PR.
- `testing` — which kind of test a change needs, what needs a live
  PostgreSQL, and the gotchas that produce false-positive passes.
- `review` — the review checklist for plans and finished branches.
- `build-feature` — the end-to-end plan/implement/review pipeline for a
  substantial feature.
