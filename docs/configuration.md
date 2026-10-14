# Configuration Guide

## Config File Structure

A benchmark config is a TOML file with these sections:

```toml
[dataset]
name = "cohere-1m"
path = "~/data/cohere-wikipedia-22-12-1M-angular.hdf5"
metric = "angular"       # angular or euclidean
dimension = 768

[connection]
dsn = "host=/tmp port=5432 dbname=postgres"

[load]
table = "cohere_1m"      # defaults to dataset name with - replaced by _
storage = "auto"          # heap column type: auto | vector | halfvec
parallelism = 1           # COPY streams (Phase 2)
chunk_size = 50000        # vectors per HDF5 read

[[index]]
type = "prism"
name = "prism-fs16"
[index.params]            # passed to CREATE INDEX ... WITH (...)
nlist = 1000
fastscan = true
[index.gucs]              # GUCs set before every search sweep
"prism.fastscan_bits" = 16

[[index]]
type = "pgvector-hnsw"
name = "hnsw-m16"
[index.params]
m = 16
ef_construction = 200

[[index]]
type = "pgvector-ivfflat"
name = "ivfflat-l3162"
[index.params]
lists = 3162               # pgvector guidance: sqrt(rows) beyond 1M rows
# encoding = "halfvec"     # or "bit"; both support mode = "rerank" queries

[search]
k = 10
runs = 3                  # best-of-N runs kept
warmup = 100              # warmup queries before measurement
num_queries = 2000        # omit to use all queries in dataset
clients = ["1x", "2x"]    # concurrent clients; see "Concurrent Clients"
concurrency_recall_targets = [0.95, 0.99]

[[search.sweep]]
index = "prism-fs16"
[search.sweep.params]     # keys are PG GUC names
"prism.nprobe" = [5, 10, 20, 40, 80]

[[search.sweep]]
index = "hnsw-m16"
[search.sweep.params]
"hnsw.ef_search" = [10, 20, 40, 80, 200, 400]

[output]
format = ["table", "json"]
results_dir = "results/cohere-1m"
```

Each searched index writes a timestamped `{index}-{ts}.json.gz` under
`results_dir` (the source of truth), and its concurrent-client results go
in a separate `{index}-concurrency-{ts}.json.gz`. The consolidated view is
produced on demand by `rekall report <results_dir>` /
`rekall publish <results_dir>`.

## Index Params vs GUCs

Each `[[index]]` has two parameter sections:

- **`[index.params]`** — build-time parameters passed to
  `CREATE INDEX ... WITH (...)`. These are index-type-specific (e.g.,
  `nlist`, `fastscan` for pg_vectorsearch; `m`, `ef_construction` for pgvector).
  A key the index type does not implement is an error at load time, including
  `--dry-run`, rather than a silently ignored option. The same applies to
  unknown keys in suite, dataset, and index files.

- **`[index.gucs]`** — default GUC values set before every search sweep.
  Keys must be full PG GUC names (e.g., `"prism.fastscan_bits"`, not
  `fastscan_bits`). These are applied before each sweep point; sweep
  params in `[search.sweep.params]` override them.

## Search Sweep Params

Sweep params use full PG GUC names as keys:

```toml
[[search.sweep]]
index = "prism-fs16"
[search.sweep.params]
"prism.nprobe" = [5, 10, 20, 40, 80]
```

Multiple params create a Cartesian product:

```toml
[search.sweep.params]
"prism.nprobe" = [10, 40]
"prism.fastscan_bits" = [8, 16]
# Produces 4 combinations: (10,8), (10,16), (40,8), (40,16)
```

GUC names are validated before the sweep starts. A typo like
`prism.nprob` gives a clear error instead of silently using the wrong
parameter.

## Concurrent Clients

After a query series' single-client sweep, rekall measures it again with
several clients querying at once. This runs only at the operating point
for each recall target: the highest-QPS sweep point whose recall reaches
the target. Recall is distance-based when the dataset has ground-truth
distances, the same basis the published reports use.

| Key | Default | Meaning |
|-----|---------|---------|
| `clients` | `["1x", "2x"]` | Client counts to measure. `"Nx"` is N times the CPU count of the host running rekall (fractions like `"1.5x"` allowed, up to `"64x"`); plain integers are fixed counts, up to 10,000. `[]` turns concurrency off. |
| `concurrency_recall_targets` | `[0.95, 0.99]` | Recall targets whose operating points are measured. A target no sweep point reaches is skipped with a warning. |

Counts that resolve to the same number run once. A count of 1 is covered
by the single-client baseline measured right before the concurrent runs,
and is not measured on its own: with only `clients = [1]`, or `["1x"]` on
a one-CPU host, nothing runs. The CPU count is the rekall host's, which
matches the database's when both run on one machine, as `rekall infra`
sets them up; use plain integers when PostgreSQL runs elsewhere.

PostgreSQL's `max_connections` must leave room for the largest count plus
rekall's own few connections; `rekall infra` provisions `2 × vCPU + 50`.
A count that does not fit, by the server's count of free slots or because
the server refuses a client connection, is skipped with a warning and the
run goes on. Each client is a separate backend with its own memory, which
comes out of what the OS can use as page cache.

Each operating point adds a baseline and one run per count, each of
`search.runs` repetitions over the query set, cycled until there are at
least ten queries per client. That is a few points per query series, small
next to the sweep itself, though a high-recall baseline can take minutes
on a large index.

Each measurement reports two throughputs. `qps_wall` is every query over
the whole run, and it is what the reports lead with and what picks the
best of `search.runs`. `throughput_qps` adds up each client's own rate over
the queries it finished before the first client ran out of work, so it
leaves out the drain at the end, when fewer clients are still busy. Close
together, they say the run was long enough for the client count.

Rerunning on a results directory measures only what is missing. A
series' newest concurrency file is its complete set: when the sweep or
the targets change which points are selected, results for points no
longer selected are dropped from it, and results taken with a different
`search.runs` or `search.cache` are measured again.

## Local Overrides

Every config file `foo.toml` automatically checks for `foo.local.toml`
in the same directory. The local file is deep-merged on top of the base:

- Tables merge recursively (nested keys are combined)
- Scalars and arrays in the local file replace the base values

Local files are gitignored (`*.local.toml` in `.gitignore`), so
temporary changes never get committed accidentally.

**Example**: temporarily reduce queries for faster iteration:

```toml
# cohere-1m.local.toml
[search]
runs = 1
num_queries = 100
```

This overrides `runs` and `num_queries` while keeping everything else
from the base config.

**Verify the merged result**:

```bash
rekall config benchmarks/datasets/cohere-1m.toml
```

## Suite Files

A **suite file** pairs one or more datasets with a shared set of index
definitions, so a single `rekall run` invocation can cross datasets ×
index variants. The committed suites live in `benchmarks/` — that's
the convention, not a requirement. You can put your own suite + index
+ dataset files anywhere and pass the path explicitly to `rekall run`.

```toml
# benchmarks/cohere-1m.toml
indexes = [
    "indexes/prism-defaults.toml",
    "indexes/hnsw-m24-halfvec.toml",
]

[[suite]]
dataset = "datasets/cohere-1m.toml"

[[suite]]
dataset = "datasets/cohere-10m.toml"
indexes = ["indexes/prism-defaults.toml"]   # override the default per-suite
```

Paths in a suite file are resolved relative to the suite's parent
directory. Each `[[suite]]` entry produces one benchmark group; the
default `indexes = [...]` at the top is the per-entry default, which
any single entry can override.

Running:

```bash
# Canonical suites
rekall run benchmarks/cohere-1m.toml
rekall run benchmarks/cohere-10m-prism-vs-vchordrq.toml

# A suite you keep outside the repo
rekall run ~/my-experiments/scaling-test.toml
```

Repo layout under `benchmarks/`:

```
benchmarks/
  cohere-{1,5,10,50,100}m.toml     # base suites: default PRISM vs hnsw-m24-halfvec
  cohere-10m-prism-vs-vchordrq.toml
  cohere-100m-ivfflat.toml
  cohere-{10,50,100}m-tuning.toml
  datasets/cohere-{1m,10m,...}.toml
  indexes/{hnsw,prism,vchordrq}-*.toml
```

## Directory-Based Config Groups

Instead of one large config file, organize variants in a directory:

```
configs/cohere-1m/
  base.toml              # dataset, connection, search defaults
  01-prism-vanilla.toml    # [[index]] + [[search.sweep]]
  02-prism-fastscan.toml   # [[index]] + [[search.sweep]]
  03-hnsw-m16.toml       # [[index]] + [[search.sweep]]
```

`base.toml` provides `[dataset]`, `[connection]`, `[search]`, and
`[output]`. Each other file adds `[[index]]` and `[[search.sweep]]`
entries. Files are loaded in sorted order (use numeric prefixes to
control order).

### Running a Directory

```bash
# All configs in the directory
rekall run configs/cohere-1m-comparison/

# Only PRISM configs (shell glob)
rekall run configs/cohere-1m-comparison/prism-*.toml

# Multiple groups
rekall run configs/cohere-1m-comparison/ configs/cohere-10m-comparison/prism-*.toml
```

When individual files are specified, `base.toml` is still loaded from
their parent directory for dataset/connection defaults.

### Local Configs in Directories

Add gitignored exploration configs directly in the directory:

```
configs/cohere-1m/
  base.toml
  01-prism-vanilla.toml       # committed
  02-prism-fastscan.toml      # committed
  99-experiment.local.toml  # gitignored, temporary
```

Local files are loaded alongside regular files when running the full
directory.

### Index Fragment Format

Each index config file in a directory contains:

```toml
[[index]]
type = "prism"
name = "prism-vanilla"
[index.params]
nlist = 1000

[index.gucs]
"prism.fastscan_bits" = 16

[[search.sweep]]
index = "prism-vanilla"
[search.sweep.params]
"prism.nprobe" = [5, 10, 20, 40, 80]
```

## Index Reuse

When multiple index configs are defined, rekall:

1. **Builds** each index that doesn't already exist (skips existing ones)
2. **Disables** all other indexes on the column
3. **Enables** only the active index
4. **Runs** the search sweeps
5. **Re-enables** all indexes when done

This avoids expensive rebuilds when re-running benchmarks with the same
index configs. Use `--rebuild` to force rebuilding all indexes:

```bash
rekall run configs/cohere-1m-comparison/ --rebuild
```

## Run Traceability

Every run captures and outputs:

- **Effective config**: the full merged config (base + local overrides)
- **PG version**: PostgreSQL server version string
- **PG settings**: shared_buffers, work_mem, effective_cache_size, etc.
- **Extensions**: installed extensions with versions, plus library
  version and git commit if the extension provides version functions

This information is printed to stdout at the start of each run and
included in JSON output under `config` and `pg_runtime` keys.

## CLI Reference

```
rekall run <suite-or-paths...>       # all phases
  --datasets <files...> --indexes <files-or-dirs...>
  --phase init|load|index|search|report
  --index <name>                     # only this index
  --rebuild                          # rebuild indexes that already exist
  --reload                           # drop and reload the table
  --clean-run                        # archive the results dir, then start fresh
  --publish                          # upload to S3 and publish pages
  --comment "..."
  --dry-run

rekall init <config>
rekall load <config> [--parallelism N]
rekall index <config> [--name N]
rekall search <config> [--index N] [--rerun]
rekall report <results-dir> [--dataset <hdf5>] [-o <dir>]
rekall publish <results-dir>         # --dry-run, --force, --s3-only, --no-pages
rekall recall <results...> --ground-truth <hdf5-or-json>
rekall plot <results...> -o plot.svg

rekall config [<config>]
rekall setup                         # ~/.config/rekall/config.toml
rekall dataset info|validate|truth|sql|compare <file.hdf5>

rekall infra init [-c config] [--terraform]
rekall infra up [-c config] [--spot-instance=true|false] [-n name]
rekall infra run [-c config] <benchmarks...> [--wait] [--no-publish]
rekall infra load [-c config] <benchmarks...>
rekall infra resize -c <target.toml> [-n name]
rekall infra down|status|ssh|attach|dsn [-c config] [-n name]
rekall infra list
rekall infra show [-c config]
rekall infra provision [-c config]
rekall infra deploy-key [-c config]
rekall infra iam-policy [-c config] [--cloudformation]
```

`rekall report` and `rekall publish` take a results directory of per-index
`*.json.gz` files, not a benchmark config. `rekall search` skips sweep
points already saved in that directory; `--rerun` measures them again.

## Heap storage precision (`[load] storage`)

The type of the table's `v` column, independent of any index arm's own
`encoding` (a pgvector halfvec expression index over a `halfvec` heap is just
a no-op cast).

| value | column |
| --- | --- |
| `auto` (default) | matches the source file: `halfvec` for an f16 dataset, `vector` otherwise |
| `vector` | `vector(N)`, f32 |
| `halfvec` | `halfvec(N)`, f16 |

**Why it matters.** The heap is what an exact rerank reads. At 768d a `vector`
row is 3080 bytes and fits 2 to an 8 kB page; a `halfvec` row is 1544 and fits
5. Halving the heap halves both the bytes competing with the index for cache
and the pages a rerank touches — which is the whole difference between
graceful degradation and a cliff on a memory-constrained box. For cohere-1M
the PRISM working set is index 0.26 GiB + heap 3.81 GiB; a halfvec heap
takes that to ~2.2 GiB, against HNSW-halfvec's 1.91 GiB index.

**Why `auto` exists.** HDF5 converts inside `H5Dread`, so rekall always
receives `f32` whatever the file holds — the file's own precision is invisible
to the loader unless it is asked for. Widening an f16 source into a `vector`
column therefore happened silently, storing bit-identical values in twice the
space: identical recall, twice the heap. `auto` reads the `train` dataset's
element type from the HDF5 header and matches it.

`auto` never *narrows*: an f64 source (glove) already loses precision reaching
f32, and going on to f16 is a real tradeoff, so it stays an explicit opt-in.
`rekall dataset info` prints the detected precision, and every run records it
in `DatasetMeta.source_precision`.

As of this writing every cohere file in `ann-benchmarks/data/` is
`H5T_IEEE_F32LE` and glove is `H5T_IEEE_F64LE` — none are f16 — so `auto`
resolves to `vector` for all of them and the setting only takes effect when
written explicitly.

**Table naming.** An explicit `halfvec` gets its own table (`cohere_1m_halfvec`)
so f32 and f16 can be loaded side by side on one instance and compared;
sharing a name would make the second load truncate the first. The suffix is
keyed to the *declared* setting, not the resolved type, so a table name never
depends on opening the dataset file — which is why `auto` always takes the
bare name.

**Type drift.** An existing table is reused rather than recreated, and the
reuse check compares only row counts. A run whose `storage` resolves to a type
the table's `v` column is not now fails up front instead of silently measuring
the wrong heap; `--reload` recreates the table.

**Caveat.** PRISM's `prism` opclasses are declared `FOR TYPE vector` only,
so a PRISM arm cannot index a `halfvec` column. The option currently serves
pgvector arms.
