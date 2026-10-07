# Rekall

**Rekall** is a vector search benchmark tool built in Rust. It measures the
recall-vs-throughput tradeoff across PostgreSQL vector indexes — PRISM,
pgvector (HNSW and IVFFlat), and VectorChord.

## Why "Rekall"?

The name is a nod to **Rekall Inc.** from the 1990 film *Total Recall* -- the
company that implants memories so vivid you can't tell them from the real thing.
In approximate nearest neighbor (ANN) search, **recall** is the metric that
measures exactly that: how many of the *true* nearest neighbors did you actually
find? Total recall means you found them all.

## Key Features

- **Phase-based execution**: init, load, index, search, and report run
  independently. Load a dataset once, then benchmark multiple index types.
- **Suite files**: one TOML file pairs datasets with shared index definitions.
  A single `rekall run` crosses datasets × index variants. Directory groups
  and `*.local.toml` overrides still work for one-off exploration.
- **Memory-bounded loading**: streams HDF5 in chunks via COPY BINARY, so
  memory stays bounded as the dataset grows. Missing files are downloaded
  from the dataset `uri` (S3 or HTTP).
- **Index reuse**: indexes are built once and switched with enable/disable.
  Existing indexes are skipped unless `--rebuild`; a loaded table is kept
  unless `--reload`. Completed search points are skipped on the next run.
- **Prepared statements**: binary vector encoding with prepared queries for
  minimal measurement overhead.
- **Reports**: recall-vs-QPS (Pareto and per-config), build times, and a
  summary table, as HTML and SVG, from a results directory.
- **Publishing**: archive a run to S3 and publish GitHub Pages reports.
  Off by default locally; on by default for `rekall infra run`.
- **Full traceability**: every run records the effective config, a comment,
  PostgreSQL version and settings, and extension versions.
- **Cloud infrastructure**: provision AWS EC2 with PostgreSQL, extensions,
  and rekall via `rekall infra`. Two backends: AWS CLI (default) or Terraform.

## Requirements

- Rust, as pinned in `rust-toolchain.toml`
- libhdf5 (the `hdf5-metno` crate, HDF5 2.x)
- PostgreSQL with pgvector, plus whichever index extensions the config builds

## Quick Start

```bash
# Build
cargo build --release

# Optional: write ~/.config/rekall/config.toml (DSN, data dir, results dir)
rekall setup

# Inspect a dataset
rekall dataset info data/cohere-wikipedia-22-12-1M-angular.hdf5

# Preview, then run a suite
rekall run benchmarks/cohere-1m.toml --dry-run
rekall run benchmarks/cohere-1m.toml

# One dataset against a chosen set of indexes
rekall run --datasets benchmarks/datasets/cohere-1m.toml \
           --indexes benchmarks/indexes/hnsw-m16.toml \
                     benchmarks/indexes/mkt-n4000-cctrue-fs16-soar.toml

# Directory group (base.toml + every index file in the directory)
rekall run configs/cohere-1m-comparison/

# Subset of a directory (shell glob)
rekall run configs/cohere-1m-comparison/mkt-*.toml

# Show the effective merged config, or list discovered configs
rekall config benchmarks/datasets/cohere-1m.toml
rekall config

# Existing indexes and a populated table are reused.
# Force a rebuild or a reload explicitly.
rekall run benchmarks/cohere-1m.toml --rebuild
rekall run benchmarks/cohere-1m.toml --reload
```

Committed suites live under `benchmarks/`. A suite file is the usual entry
point; the path can be anywhere.

## Commands

```text
rekall run <suite-or-paths...>       # all phases
  --datasets <files...> --indexes <files-or-dirs...>
  --phase init|load|index|search|report
  --index <name>                     # one index
  --rebuild                          # rebuild indexes that already exist
  --reload                           # drop and reload the table
  --clean-run                        # archive the results dir, then start fresh
  --publish                          # upload to S3 and publish pages
  --comment "..."                    # recorded on reports and published pages
  --dry-run

rekall init <config>                 # create database objects
rekall load <config> [--parallelism N]
rekall index <config> [--name N]     # build indexes
rekall search <config> [--index N] [--rerun]
rekall report <results-dir>          # summary + HTML/SVG plots
rekall publish <results-dir>         # archive + pages (see --dry-run, --s3-only)
rekall recall <results...> --ground-truth <hdf5-or-json>
rekall plot <results...> -o plot.svg

rekall dataset info <file.hdf5>
rekall dataset validate <file.hdf5>
rekall dataset truth <file.hdf5>     # ground-truth neighbours, offline
rekall dataset sql <file.hdf5>       # exact query SQL, offline
rekall dataset compare <file.hdf5>   # SQL that diffs results vs ground truth

rekall config [<path>]
rekall setup                         # ~/.config/rekall/config.toml
```

`rekall search` resumes by default: sweep points already saved in the
results directory are skipped. Pass `--rerun` to measure them again.
`rekall run` does the same unless `--clean-run` moves that directory aside.

Each searched index writes `{index}-{timestamp}.json.gz` under
`results_dir`. `rekall report` and `rekall publish` read that directory;
they do not take a benchmark config.

## Cloud Benchmarks

```bash
# One-time: IAM template, deploy key, machine defaults
rekall infra init -c infra/instances/r7g.2xlarge.toml

# Provision (installs PostgreSQL, extensions, rekall)
rekall infra up -c infra/instances/r7g.2xlarge.toml

# Upload the suite and run it in tmux. Publishes when the run finishes.
rekall infra run -c infra/instances/r7g.2xlarge.toml benchmarks/cohere-1m.toml

# Watch the session, or just SSH in
rekall infra attach -c infra/instances/r7g.2xlarge.toml
rekall infra ssh -c infra/instances/r7g.2xlarge.toml

# Build on a large box, then shrink in place for an out-of-RAM query run
rekall infra resize -c infra/instances/r8g.2xlarge.toml

rekall infra list
rekall infra down -c infra/instances/r7g.2xlarge.toml
```

`infra run` turns publishing on. Pass `--no-publish` to keep results on the
instance. `--wait` blocks until the remote run exits, which is what you want
before a later `infra resize`.

See [Cloud Infrastructure](docs/cloud-infrastructure.md) for IAM, keys,
instance types, and the rest of the `infra` commands (`load`, `status`,
`dsn`, `show`, `provision`).

## Documentation

- [Configuration Guide](docs/configuration.md) — config files, suites,
  local overrides, directory groups, GUC defaults
- [Cloud Infrastructure](docs/cloud-infrastructure.md) — AWS provisioning,
  SSH keys, instance types
- [benchmarks/README.md](benchmarks/README.md) — suite layout and how to
  run a subset

## License

TBD
