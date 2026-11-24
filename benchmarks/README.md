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

### Run a single dataset with the default comparison indexes

```bash
rekall run --datasets benchmarks/datasets/cohere-1m.toml \
           --indexes benchmarks/indexes/prism-defaults.toml \
                     benchmarks/indexes/hnsw-m24-halfvec.toml
```

### Run specific combinations

```bash
rekall run --datasets benchmarks/datasets/cohere-1m.toml \
                      benchmarks/datasets/cohere-10m.toml \
           --indexes benchmarks/indexes/hnsw-m24-halfvec.toml \
                     benchmarks/indexes/prism-defaults.toml
```

## MS MARCO 113M: pg_vectorsearch vs. ParadeDB

[`msmarco-113m-prism-vs-paradedb.toml`](msmarco-113m-prism-vs-paradedb.toml)
uses the [ParadeDB blog's MS MARCO workload](https://www.paradedb.com/blog/filter-aware-vector-search):
113,520,750 embeddings of dimension 1024, cosine distance, no filters,
and the same 100 held-out queries (ascending query ID). Both engines run
`LIMIT 10` and `LIMIT 100`. Each limit has its own results directory, but
both reuse the same table and physical indexes. Run without `--reload`
or `--rebuild` to reuse them across the two groups. On a fresh run,
the top-10 group's results include the build timings.

**Prepare the input once.** ParadeDB publishes Parquet, not Rekall's
HDF5 format. The converter streams the public corpus and both published
ground-truth files, maps document IDs to Rekall's row IDs, and checks
that the top-10 fixture is the prefix of the top-100 fixture. Allow
about 433 GiB for the resulting HDF5 file. On the benchmark host, from
a checkout of this repository:

```bash
sudo apt-get install -y python3-venv  # on Ubuntu, if needed
python3 -m venv ~/msmarco-venv
~/msmarco-venv/bin/pip install h5py numpy pyarrow
~/msmarco-venv/bin/python scripts/prepare-msmarco.py \
  ~/data/msmarco-113m-1024-angular.hdf5
```

**AWS target.** Use
[`msmarco-113m-m8g.2xlarge.toml`](../infra/instances/msmarco-113m-m8g.2xlarge.toml):
Graviton4/ARM64, 8 vCPUs, 32 GiB of RAM, and the published PostgreSQL
memory and worker settings. It enables pgvector, pg_vectorsearch and
ParadeDB. Set `[rekall]` to a git ref containing the ParadeDB backend
in your infra overrides. The instance profile pins pg_vectorsearch to
`93567fd1a10096a28f11a247151fe5acae8ee2d9`, the revision used by
[ParadeDB's benchmark profile](https://github.com/paradedb/paradedb/blob/main/.github/workflows/benchmark-pg_search-benchmarker-msmarco.yml),
including its parallel-build memory fix.

```bash
rekall infra up -c infra/instances/msmarco-113m-m8g.2xlarge.toml
# Prepare the HDF5 file on the instance before running the suite.
rekall infra run -c infra/instances/msmarco-113m-m8g.2xlarge.toml \
  benchmarks/msmarco-113m-prism-vs-paradedb.toml --wait --no-publish
```

This matches the container's CPU architecture and resource budget, not
its storage or isolation: the blog used local NVMe on a
`c8gd.metal-24xl`, while this profile uses a 4,000 GiB gp3 volume to fit
the HDF5 file, heap, both indexes and build space. Rekall runs on the same
instance, returns only IDs, and sweeps fixed probe values with one
client. The blog instead tuned to recall targets and timed a separate
50-second k6 run. Compare curves at matched recall, not raw QPS against
the blog.

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
