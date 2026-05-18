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

## Why Not ann-benchmarks?

[ann-benchmarks](https://github.com/erikbern/ann-benchmarks) is the standard
ANN benchmark suite but has limitations at scale:

- **Memory**: loads entire datasets into Python/NumPy before passing to
  algorithms. A 100M-vector dataset at 768 dimensions needs ~286 GB of RAM
  just for the array.
- **Monolithic runs**: rebuilds everything end-to-end. Loading 100M vectors
  into PostgreSQL takes hours; you don't want to redo it for each index config.
- **Docker-only**: forces algorithm isolation via Docker containers, which adds
  overhead and limits access to host resources.

Rekall fixes these by operating in **independent phases** with **streaming
data loading** and **shared tables across index types**.

## Key Features

- **Phase-based execution**: init, load, index, search, and report run
  independently. Load a dataset once, then benchmark multiple index types
  against it.
- **Memory-bounded loading**: streams HDF5 data in chunks via parallel COPY
  BINARY. Loading 100M vectors uses ~600 MB of working memory, not 286 GB.
- **Resumable loads**: tracks progress in a metadata table. If loading
  crashes at row 50M, it picks up where it left off.
- **Pareto exploration**: sweeps index build parameters and query-time
  parameters to map the recall-vs-QPS frontier, similar to ann-benchmarks.
- **Shared data, multiple indexes**: PostgreSQL-based indexes share the same
  loaded table. Compare Meerkat vs pgvector HNSW without loading twice.
- **Pluggable output**: stdout tables, JSON export, and (future) PostgreSQL
  result storage for historical comparison.
- **Actionable errors**: every failure tells you what went wrong and which
  command to run to fix it.

## Usage

```bash
# Inspect a dataset
rekall dataset info cohere-wikipedia-22-12-1M-angular.hdf5

# Run a full benchmark
rekall run cohere-1m.toml

# Run individual phases
rekall init cohere-1m.toml
rekall load cohere-1m.toml --parallelism 4
rekall index cohere-1m.toml --name mkt-nlist-1000
rekall search cohere-1m.toml
rekall report cohere-1m.toml
```

## Configuration

Benchmarks are defined in TOML config files:

```toml
[dataset]
name = "cohere-1m"
path = "~/data/cohere-wikipedia-22-12-1M-angular.hdf5"
metric = "angular"
dimension = 768

[connection]
dsn = "host=/tmp port=5432 dbname=postgres"

[load]
table = "cohere_1m"
parallelism = 4
chunk_size = 50000

[[index]]
type = "meerkat"
name = "mkt-nlist-1000"
[index.params]
nlist = 1000
fan_out = 32

[[index]]
type = "pgvector-hnsw"
name = "hnsw-m16"
[index.params]
m = 16
ef_construction = 200

[search]
k = 10
runs = 3
warmup = 100

[[search.sweep]]
index = "mkt-nlist-1000"
[search.sweep.params]
nprobe = [5, 10, 20, 40, 80]

[[search.sweep]]
index = "hnsw-m16"
[search.sweep.params]
ef_search = [10, 20, 40, 80, 200, 400]
```

## Roadmap

See [DESIGN.md](DESIGN.md) for the full architecture and implementation plan.

## License

TBD
