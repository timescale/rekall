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
name = "mkt-fs16"
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

[[search.sweep]]
index = "mkt-fs16"
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
`results_dir` (the source of truth). The consolidated view is produced on
demand by `rekall report <results_dir>` / `rekall publish <results_dir>`.

## Index Params vs GUCs

Each `[[index]]` has two parameter sections:

- **`[index.params]`** — build-time parameters passed to
  `CREATE INDEX ... WITH (...)`. These are index-type-specific (e.g.,
  `nlist`, `fastscan` for pg_vectorsearch; `m`, `ef_construction` for pgvector).

- **`[index.gucs]`** — default GUC values set before every search sweep.
  Keys must be full PG GUC names (e.g., `"prism.fastscan_bits"`, not
  `fastscan_bits`). These are applied before each sweep point; sweep
  params in `[search.sweep.params]` override them.

## Search Sweep Params

Sweep params use full PG GUC names as keys:

```toml
[[search.sweep]]
index = "mkt-fs16"
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
# benchmarks/run-all.toml
indexes = [
    "indexes/hnsw-m16.toml",
    "indexes/mkt-n4000-cctrue-fs16-soar.toml",
    "indexes/vchordrq-rabitq8.toml",
]

[[suite]]
dataset = "datasets/cohere-1m.toml"

[[suite]]
dataset = "datasets/cohere-10m.toml"
indexes = ["indexes/hnsw-m16.toml"]   # override the default per-suite
```

Paths in a suite file are resolved relative to the suite's parent
directory. Each `[[suite]]` entry produces one benchmark group; the
default `indexes = [...]` at the top is the per-entry default, which
any single entry can override.

Running:

```bash
# Canonical suites
rekall run benchmarks/run-all.toml
rekall run benchmarks/vchordrq-rabitq8.toml

# A suite you keep outside the repo
rekall run ~/my-experiments/scaling-test.toml
```

Repo layout under `benchmarks/`:

```
benchmarks/
  run-all.toml                     # suite: all indexes × all datasets
  vchordrq-*.toml                  # per-variant suites
  datasets/cohere-{1m,10m,...}.toml
  indexes/{hnsw,mkt,vchordrq}-*.toml
```

## Directory-Based Config Groups

Instead of one large config file, organize variants in a directory:

```
configs/cohere-1m/
  base.toml              # dataset, connection, search defaults
  01-mkt-vanilla.toml    # [[index]] + [[search.sweep]]
  02-mkt-fastscan.toml   # [[index]] + [[search.sweep]]
  03-hnsw-m16.toml       # [[index]] + [[search.sweep]]
```

`base.toml` provides `[dataset]`, `[connection]`, `[search]`, and
`[output]`. Each other file adds `[[index]]` and `[[search.sweep]]`
entries. Files are loaded in sorted order (use numeric prefixes to
control order).

### Running a Directory

```bash
# All configs in the directory
rekall run configs/cohere-1m/

# Only pg_vectorsearch configs (shell glob)
rekall run configs/cohere-1m/mkt-*.toml

# Multiple groups
rekall run configs/group1/ configs/group2/mkt-*.toml
```

When individual files are specified, `base.toml` is still loaded from
their parent directory for dataset/connection defaults.

### Local Configs in Directories

Add gitignored exploration configs directly in the directory:

```
configs/cohere-1m/
  base.toml
  01-mkt-vanilla.toml       # committed
  02-mkt-fastscan.toml      # committed
  99-experiment.local.toml  # gitignored, temporary
```

Local files are loaded alongside regular files when running the full
directory.

### Index Fragment Format

Each index config file in a directory contains:

```toml
[[index]]
type = "prism"
name = "mkt-vanilla"
[index.params]
nlist = 1000

[index.gucs]
"prism.fastscan_bits" = 16

[[search.sweep]]
index = "mkt-vanilla"
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
rekall run configs/cohere-1m/ --rebuild
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
rekall run <paths...>                # run benchmark(s)
  --phase <phase>                    # run only: init, load, index, search, report
  --index <name>                     # only this index
  --rebuild                          # force rebuild existing indexes
  --dry-run                          # show what would run

rekall init <config>                 # create tables only
rekall load <config>                 # load dataset only
  --parallelism <N>
rekall index <config>                # build indexes only
  --name <name>
rekall search <config>               # search only
  --index <name>
rekall report <config>               # generate report

rekall config [<config>]             # show config(s)
rekall dataset info <file.hdf5>      # inspect dataset
rekall dataset validate <file.hdf5>  # validate dataset

rekall infra init [-c config]         # generate setup resources + instructions
rekall infra up [-c config] [--spot-instance=true|false]  # provision cloud instance
rekall infra down [-c config] [-n N]  # destroy cloud instance
rekall infra list                     # list all instances
rekall infra status [-c config] [-n N]# show instance status
rekall infra ssh [-c config] [-n N]   # SSH into instance
rekall infra dsn [-c config] [-n N]   # print connection string
rekall infra deploy-key [-c config]   # generate deploy key for GitHub
rekall infra iam-policy [-c config]   # print IAM template
```

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
