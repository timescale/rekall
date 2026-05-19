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
parallelism = 1           # COPY streams (Phase 2)
chunk_size = 50000        # vectors per HDF5 read

[[index]]
type = "meerkat"
name = "mkt-fs16"
[index.params]            # passed to CREATE INDEX ... WITH (...)
nlist = 1000
fastscan = true
[index.gucs]              # GUCs set before every search sweep
"mkt.fastscan_bits" = 16

[[index]]
type = "pgvector-hnsw"
name = "hnsw-m16"
[index.params]
m = 16
ef_construction = 200

[search]
k = 10
runs = 3                  # best-of-N runs kept
warmup = 100              # warmup queries before measurement
num_queries = 2000        # omit to use all queries in dataset

[[search.sweep]]
index = "mkt-fs16"
[search.sweep.params]     # keys are PG GUC names
"mkt.nprobe" = [5, 10, 20, 40, 80]

[[search.sweep]]
index = "hnsw-m16"
[search.sweep.params]
"hnsw.ef_search" = [10, 20, 40, 80, 200, 400]

[output]
format = ["table", "json"]
json_path = "results/cohere-1m.json"
```

## Index Params vs GUCs

Each `[[index]]` has two parameter sections:

- **`[index.params]`** — build-time parameters passed to
  `CREATE INDEX ... WITH (...)`. These are index-type-specific (e.g.,
  `nlist`, `fastscan` for meerkat; `m`, `ef_construction` for pgvector).

- **`[index.gucs]`** — default GUC values set before every search sweep.
  Keys must be full PG GUC names (e.g., `"mkt.fastscan_bits"`, not
  `fastscan_bits`). These are applied before each sweep point; sweep
  params in `[search.sweep.params]` override them.

## Search Sweep Params

Sweep params use full PG GUC names as keys:

```toml
[[search.sweep]]
index = "mkt-fs16"
[search.sweep.params]
"mkt.nprobe" = [5, 10, 20, 40, 80]
```

Multiple params create a Cartesian product:

```toml
[search.sweep.params]
"mkt.nprobe" = [10, 40]
"mkt.fastscan_bits" = [8, 16]
# Produces 4 combinations: (10,8), (10,16), (40,8), (40,16)
```

GUC names are validated before the sweep starts. A typo like
`mkt.nprob` gives a clear error instead of silently using the wrong
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
rekall config examples/cohere-1m.toml
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

# Only meerkat configs (shell glob)
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
type = "meerkat"
name = "mkt-vanilla"
[index.params]
nlist = 1000

[index.gucs]
"mkt.fastscan_bits" = 16

[[search.sweep]]
index = "mkt-vanilla"
[search.sweep.params]
"mkt.nprobe" = [5, 10, 20, 40, 80]
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
```
