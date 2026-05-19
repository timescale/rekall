# Rekall

*"Get your recall back."*

**Rekall** is a vector search benchmark tool built in Rust. It measures the
recall-vs-throughput tradeoff across PostgreSQL vector indexes (Meerkat,
pgvector, VectorChord) and, in the future, remote vector search services
(Pinecone, Turbopuffer, etc.).

## Why "Rekall"?

The name is a nod to **Rekall Inc.** from the 1990 film *Total Recall* -- the
company that implants memories so vivid you can't tell them from the real thing.
In approximate nearest neighbor (ANN) search, **recall** is the metric that
measures exactly that: how many of the *true* nearest neighbors did you actually
find? Total recall means you found them all.

## Key Features

- **Phase-based execution**: init, load, index, search, and report run
  independently. Load a dataset once, then benchmark multiple index types.
- **Memory-bounded loading**: streams HDF5 data in chunks via COPY BINARY.
  Loading 100M vectors uses ~150 MB per stream, not 286 GB.
- **Multi-config exploration**: define multiple index configs and sweep them
  in one run. Indexes are built once and reused via enable/disable switching.
- **Directory-based config groups**: organize configs in directories, run
  them individually or with shell globs.
- **Local overrides**: `*.local.toml` files override base configs without
  being committed to the repo.
- **Prepared statements**: binary vector encoding with prepared queries for
  minimal measurement overhead.
- **Full traceability**: every run captures the effective config, PG version,
  settings, and extension versions in both stdout and JSON output.

## Quick Start

```bash
# Build
cargo build --release

# Inspect a dataset
rekall dataset info data/cohere-wikipedia-22-12-1M-angular.hdf5

# Run a benchmark
rekall run examples/cohere-1m.toml

# Run from a config directory
rekall run configs/cohere-1m/

# Run only specific configs (shell glob)
rekall run configs/cohere-1m/mkt-*.toml

# Run multiple groups
rekall run configs/group1/ configs/group2/mkt-*.toml

# Show effective merged config
rekall config examples/cohere-1m.toml

# List all configs
rekall config

# Skip rebuilding existing indexes
rekall run configs/cohere-1m/

# Force rebuild
rekall run configs/cohere-1m/ --rebuild
```

## Documentation

- [Configuration Guide](docs/configuration.md) — config files, local
  overrides, directory groups, GUC defaults
- [DESIGN.md](DESIGN.md) — architecture, traits, data flow, roadmap

## License

TBD
