# Benchmarks

Benchmark configurations for comparing vector index implementations across
datasets. Index definitions are shared across datasets to avoid duplication.

## Directory Structure

```
benchmarks/
├── cohere-{1,5,10,50,100}m.toml  # Base suites: default PRISM vs hnsw-m24-halfvec
├── datasets/                     # One file per dataset (connection, search, output settings)
│   ├── cohere-1m.toml
│   ├── cohere-10m.toml
│   ├── cohere-20m.toml
│   ├── cohere-50m.toml
│   └── cohere-100m.toml
└── indexes/                      # One file per index variant (index params + search sweep)
    ├── prism-defaults.toml
    └── hnsw-m24-halfvec.toml
```

**Dataset files** contain `[dataset]`, `[connection]`, `[load]`, `[search]`,
and `[output]` sections — everything except index definitions.

**Index files** contain `[[index]]` and `[[search.sweep]]` entries — the index
build parameters and the search parameter sweep points.

## Usage

### Run a base suite

The base suite at each scale compares PRISM with all index parameters
left to its auto settings against pgvector's recommended HNSW default:

```bash
rekall run benchmarks/cohere-1m.toml --dry-run   # preview
rekall run benchmarks/cohere-1m.toml              # run it
```

A suite file lists shared index fragments and pairs them with datasets.
Each `[[suite]]` entry can override the default `indexes` list:

```toml
indexes = ["indexes/prism-defaults.toml", "indexes/hnsw-m24-halfvec.toml"]

[[suite]]
dataset = "datasets/cohere-1m.toml"

[[suite]]
dataset = "datasets/cohere-100m.toml"
indexes = ["indexes/prism-defaults.toml"]   # only PRISM for 100M
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
           --indexes benchmarks/indexes/hnsw-m24-halfvec.toml \
                     benchmarks/indexes/prism-defaults.toml
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

Then add a `[[suite]]` entry for it in the suite you want to run it with.

## Adding a New Index

Create a new file in `indexes/` with the index definition and search sweep.
It will be picked up when `--indexes benchmarks/indexes/` is used, and can
be added to a suite's `indexes` list:

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

The cohere-1M ladder measures how the two engines behave as RAM shrinks:
the base suite `cohere-1m.toml` runs on `infra/instances/m8g.large.toml`
(8 GiB, the rung where HNSW still builds in memory under stock tuning),
then at every rung of the 4 / 2 GiB ladder via `rekall infra resize` — one
file, so every rung shares its cache policy and query count by
construction. The instance configs document what each rung is expected to
show.

`scripts/cost-frontier.py` applies a (recall, QPS) target to the ladder
results and reports the cheapest qualifying instance per engine, costing
instance + EBS. The target is applied here, not baked into the suites.
Accepts a local results directory or a published run URL.

All of these are driven from the workstation via `rekall infra run`, which
uploads the configs and runs `rekall run` on the instance in tmux.
`rekall load` / `index` / `search` are instance-side commands — use
`rekall infra ssh` first if you want manual phase control.
