# Rekall

*"Get your recall back."*

**Rekall** is a vector search benchmark tool built in Rust. It measures the
recall-vs-throughput tradeoff across PostgreSQL vector indexes (PRISM,
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
- **Cloud infrastructure**: provision AWS EC2 instances with PostgreSQL,
  extensions, and rekall pre-installed via `rekall infra up`. Two
  backends: AWS CLI (default, no extra dependencies) or Terraform.

## Quick Start

```bash
# Build
cargo build --release

# Inspect a dataset
rekall dataset info data/cohere-wikipedia-22-12-1M-angular.hdf5

# Run a benchmark suite
rekall run benchmarks/run-all.toml

# Run from a config directory
rekall run configs/cohere-1m/

# Run only specific configs (shell glob)
rekall run configs/cohere-1m/mkt-*.toml

# Run multiple groups
rekall run configs/group1/ configs/group2/mkt-*.toml

# Show effective merged config
rekall config benchmarks/datasets/cohere-1m.toml

# List all configs
rekall config

# Skip rebuilding existing indexes
rekall run configs/cohere-1m/

# Force rebuild
rekall run configs/cohere-1m/ --rebuild
```

## Cloud Benchmarks

Provision AWS EC2 instances with PostgreSQL and run benchmarks remotely:

```bash
# One-time: generate IAM template, deploy keys, and local config
rekall infra init -c infra/instances/r7g.2xlarge.toml
# → follow the printed instructions (apply IAM, add deploy key, edit .local.toml)

# Provision instance (installs PG, extensions, rekall)
rekall infra up -c infra/instances/r7g.2xlarge.toml

# SSH in and run benchmarks
rekall infra ssh -c infra/instances/r7g.2xlarge.toml

# List all instances
rekall infra list

# Tear down
rekall infra down -c infra/instances/r7g.2xlarge.toml
```

See [Cloud Infrastructure Guide](docs/cloud-infrastructure.md) for
full setup instructions and configuration reference.

## Documentation

- [Configuration Guide](docs/configuration.md) — config files, local
  overrides, directory groups, GUC defaults
- [Cloud Infrastructure](docs/cloud-infrastructure.md) — AWS provisioning,
  SSH keys, instance types
- [DESIGN.md](DESIGN.md) — architecture, traits, data flow, roadmap

## License

TBD
