# Benchmarks

Benchmark configurations for comparing vector index implementations across
datasets. Index definitions are shared across datasets to avoid duplication.

## Directory Structure

```
benchmarks/
├── run-all.toml          # Suite file: defines which datasets × indexes to run
├── datasets/             # One file per dataset (connection, search, output settings)
│   ├── cohere-1m.toml
│   ├── cohere-10m.toml
│   ├── cohere-20m.toml
│   ├── cohere-50m.toml
│   └── cohere-100m.toml
└── indexes/              # One file per index variant (index params + search sweep)
    ├── hnsw-m16.toml
    ├── hnsw-m24.toml
    ├── prism-n4000-cctrue-fs16-soar.toml
    └── prism-n8000-cctrue-fs16-soar.toml
```

**Dataset files** contain `[dataset]`, `[connection]`, `[load]`, `[search]`,
and `[output]` sections — everything except index definitions.

**Index files** contain `[[index]]` and `[[search.sweep]]` entries — the index
build parameters and the search parameter sweep points.

## Usage

### Run all benchmarks

```bash
rekall run benchmarks/run-all.toml --dry-run   # preview
rekall run benchmarks/run-all.toml              # run everything
```

The suite file defines the cartesian product of datasets and indexes. Each
`[[suite]]` entry can override the default `indexes` list:

```toml
indexes = ["indexes/hnsw-m16.toml", "indexes/prism-n4000-cctrue-fs16-soar.toml"]

[[suite]]
dataset = "datasets/cohere-1m.toml"

[[suite]]
dataset = "datasets/cohere-100m.toml"
indexes = ["indexes/hnsw-m16.toml"]   # only HNSW for 100M
```

### Run a single dataset with all indexes

```bash
rekall run --datasets benchmarks/datasets/cohere-1m.toml \
           --indexes benchmarks/indexes/
```

### Run specific combinations

```bash
rekall run --datasets benchmarks/datasets/cohere-1m.toml \
                      benchmarks/datasets/cohere-10m.toml \
           --indexes benchmarks/indexes/hnsw-m16.toml \
                     benchmarks/indexes/prism-n4000-cctrue-fs16-soar.toml
```

## Auto-Download

Dataset configs include a `uri` field pointing to the canonical download
source (S3 or HTTP/S). When a benchmark runs and the dataset file isn't
found locally, it is downloaded automatically with a progress bar.

```toml
[dataset]
name = "cohere-1m"
uri = "s3://vector-datasets/1M/cohere-wikipedia-22-12-1M-angular.hdf5"
```

Downloaded datasets are stored in `~/.local/share/rekall/datasets/`
by default (XDG data directory). The filename is derived from the URI.
Override the location with `data_dir`:

```toml
data_dir = "/mnt/fast-disk/datasets"
```

If the file already exists, no download occurs.

S3 downloads use the AWS SDK (respects `AWS_PROFILE`, `AWS_REGION`,
etc.). HTTP/S downloads use `reqwest`.

## Local Overrides

Connection strings and dataset locations are machine-specific. Create a
`.local.toml` sibling to override them without modifying the committed file:

```bash
# benchmarks/datasets/cohere-1m.local.toml (gitignored)
[dataset]
data_dir = "~/my-datasets"       # override download directory
uri = "https://my-mirror.example.com/cohere-1M.hdf5"  # override source
# or skip download entirely by pointing to an existing file:
path = "~/data/cohere-wikipedia-22-12-1M-angular.hdf5"

[connection]
dsn = "host=/tmp port=5434 dbname=postgres"
```

## Adding a New Dataset

Create a new file in `datasets/` with the dataset metadata. It will
automatically work with all existing index configs:

```bash
cp benchmarks/datasets/cohere-1m.toml benchmarks/datasets/glove-1m.toml
# edit name, path, metric, dimension, table name
```

Then add a `[[suite]]` entry in `run-all.toml` to include it in full runs.

## Adding a New Index

Create a new file in `indexes/` with the index definition and search sweep.
It will be picked up when `--indexes benchmarks/indexes/` is used, and can
be added to `run-all.toml`'s default `indexes` list:

```toml
# benchmarks/indexes/hnsw-m32.toml
[[index]]
type = "pgvector-hnsw"
name = "hnsw-m32"
[index.params]
m = 32
ef_construction = 200

[[search.sweep]]
index = "hnsw-m32"
[search.sweep.params]
"hnsw.ef_search" = [10, 20, 40, 80, 120, 200, 400, 800, 1000]
```

## Instance Sizing

`cohere-1m-sizing.md` works out how much RAM PRISM and pgvector HNSW each
need for cohere-1M — separately for the build and for queries — from the
index sizes published at <https://timescale.github.io/rekall/>. It backs two
suites:

- `cohere-1m-prism-vs-hnsw.toml` — loads, builds, and measures the warm
  reference frontier on `infra/instances/m8g.large.toml` (8 GiB, the rung
  where HNSW still builds in memory under stock tuning).
  The same suite runs at every rung of the 8 / 4 / 2 GiB ladder via
  `rekall infra resize` — one file, so every rung shares its cache policy
  and query count by construction.
- `scripts/cost-frontier.py` — applies a (recall, QPS) target to the ladder
  results and reports the cheapest qualifying instance per engine, costing
  instance + EBS. The target is applied here, not baked into the suites.
  Accepts a local results directory or a published run URL.

All of these are driven from the workstation via `rekall infra run`, which
uploads the configs and runs `rekall run` on the instance in tmux.
`rekall load` / `index` / `search` are instance-side commands — use
`rekall infra ssh` first if you want manual phase control.
- `cohere-1m-build-floor.toml` — rebuilds at every rung with the build
  phase's `shared_buffers` traded for `maintenance_work_mem`, to find the
  smallest box each engine can *build* on (as opposed to query on). Uses
  `[phase_pg_config]`.
