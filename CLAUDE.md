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

## Git Rules

- **Never use `git add -A`, `git add .`, or `git commit -a`**. Always stage
  files explicitly by name to avoid committing binaries, results, or local
  configs.
- Review staged changes with `git diff --staged` before committing.
- The `.gitignore` excludes `/target`, `results/`, `*.local.toml`, `/data/`,
  `/configs/`, `plot.*` — but explicit staging is still required as a safeguard.

## Dependencies

Requires system libhdf5 (uses `hdf5-metno` crate for HDF5 2.x support).
PostgreSQL with pgvector extension for vector column type.
