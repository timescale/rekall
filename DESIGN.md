# Rekall Design Document

## Overview

Rekall is a Rust-based vector search benchmark tool designed to scale from 1M
to 100M+ vectors. It operates in independent phases (init, load, index, search,
report) so that expensive operations like dataset loading and index building
aren't repeated unnecessarily. Multiple index types share the same loaded data,
and indexes are reused across runs via enable/disable switching.

## Architecture

```
                    +-----------+
                    |  CLI      |  rekall run / init / load / search / config
                    +-----+-----+
                          |
                    +-----v-----+
                    |   Agent   |  Phase orchestrator, progress events
                    +-----+-----+         Interleaved index/search cycle
                          |
          +---------------+---------------+
          |               |               |
    +-----v-----+  +-----v-----+  +------v-----+
    | DatasetRdr|  | IndexBack |  | OutputSink |
    | (HDF5)    |  | (trait)   |  | (trait)    |
    +-----------+  +-----+-----+  +------------+
                         |           StdoutSink
              +----------+           JsonSink
              |
        +-----v------+
        | PgConnection|  Shared: connect, COPY BINARY, GUCs,
        | (shared)   |  prepared queries, index enable/disable,
        +-----+------+  runtime info capture
              |
     +--------+--------+
     |                  |
+----v-----+    +-------v--------+
| Meerkat  |    | PgvectorHnsw   |
+----------+    +----------------+
```

### Crate Structure

```
rekall/
  Cargo.toml                    # Workspace manifest
  CLAUDE.md
  rekall-lib/                   # Core library
    Cargo.toml
    src/
      lib.rs                    # Agent: phase orchestration + interleaved run
      config.rs                 # TOML config, local overrides, directory groups
      error.rs                  # Actionable error types
      metrics.rs                # Recall, QPS, latency, Pareto, PgRuntimeInfo
      progress.rs               # Progress event channel
      dataset/
        mod.rs                  # DatasetReader trait
        hdf5.rs                 # Chunked HDF5 streaming reader
      backend/
        mod.rs                  # IndexBackend trait + BackendRegistry
        postgres.rs             # PgConnection: COPY, GUCs, enable/disable
        pg_vector_type.rs       # Binary vector encoding (ToSql)
        pgvector_hnsw.rs        # pgvector HNSW backend
        meerkat.rs              # Meerkat mktann backend
      phase/
        init.rs                 # Create DB, tables, metadata schema
        load.rs                 # Streaming COPY via channel
        index.rs                # Build/drop indexes
        search.rs               # Query benchmarks with parameter sweeps
        report.rs               # Aggregate results, Pareto frontier
      output/
        mod.rs                  # OutputSink trait
        stdout.rs               # Formatted tables + PG runtime info
        json.rs                 # JSON file export with full config
    tests/
      pg_integration.rs         # PG-dependent tests (REKALL_TEST_DSN)
  rekall/                       # CLI binary
    Cargo.toml
    src/
      main.rs                   # clap CLI, config resolution, group runner
  docs/
    configuration.md            # Detailed config guide
  examples/
    cohere-1m.toml              # Example benchmark config
    cohere-1m-no-soar.toml
```

## Core Abstractions

### IndexBackend

The central trait. Each implementation knows how to build an index, run
queries, and manage search-time parameters.

```rust
#[async_trait]
pub trait IndexBackend: Send + Sync {
    fn type_name(&self) -> &str;
    fn index_name(&self) -> &str;
    async fn build_index(&self) -> Result<IndexBuildResult>;
    async fn drop_index(&self) -> Result<()>;
    async fn index_size(&self) -> Result<u64>;
    async fn index_exists(&self) -> Result<bool>;
    async fn set_search_params(&self, params: &HashMap<String, toml::Value>) -> Result<()>;
    async fn validate_search_params(&self, params: &HashMap<String, Vec<toml::Value>>) -> Result<()>;
    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>>;
}
```

Backends are created via `BackendRegistry::create()`, which dispatches
by the `type` field in the config and returns `Box<dyn IndexBackend>`.

### PgConnection (Shared PG Base)

All PG backends compose a `PgConnection` which provides:

- **Connection management**: persistent client with LOAD for extension GUCs
- **Prepared queries**: `connect(order_operator, extension)` looks up the
  vector type OID and prepares a parameterized query with binary encoding
- **COPY BINARY loading**: streaming data load via channel from blocking
  HDF5 reader thread
- **GUC handling**: `set_gucs()`, `validate_gucs()` with clear error messages
- **Index management**: `enable_index()`, `disable_index()`,
  `list_rekall_indexes()` via pg_index.indisvalid
- **Runtime info**: captures PG version, key settings, extension versions,
  and probes for version/git_commit functions

Concrete backends (MeerkatBackend, PgvectorHnswBackend) own a PgConnection
and delegate common operations. They only implement index-specific DDL and
the operator class for queries.

### Binary Vector Encoding

`PgVector<'a>(&[f32])` implements `ToSql` for pgvector's binary format
(u16 dim + u16 unused + f32 array). Combined with prepared statements,
this eliminates per-query string formatting overhead — measurements match
psycopg3's `binary=True, prepare=True` within 6-11%.

### DatasetReader (Streaming HDF5)

HDF5 data is read in chunks via hyperslab selection. A blocking reader
thread sends chunks over a bounded channel to the async COPY writer,
ensuring memory usage stays at ~150 MB per stream regardless of dataset
size.

### OutputSink

Pluggable output: `StdoutSink` (formatted tables with PG runtime header),
`JsonSink` (full results with config and pg_runtime for traceability).

## Configuration System

### Layered Config

1. **Base config** (committed): `examples/cohere-1m.toml`
2. **Local overrides** (gitignored): `examples/cohere-1m.local.toml`
   Deep-merged on top — tables merge recursively, scalars/arrays replace.

### Directory Groups

```
configs/cohere-1m/
  base.toml              # dataset, connection, search defaults
  01-mkt-vanilla.toml    # [[index]] + [[search.sweep]] fragment
  02-mkt-fastscan.toml   # [[index]] + [[search.sweep]] fragment
  03-hnsw-m16.toml       # [[index]] + [[search.sweep]] fragment
```

`rekall run configs/cohere-1m/` loads base.toml then appends index configs.
Shell globs select subsets: `rekall run configs/cohere-1m/mkt-*.toml`.
Multiple paths create separate groups with independent results.

### Two-Level Parameters

- **`[index.params]`**: build-time, passed to `CREATE INDEX WITH (...)`
- **`[index.gucs]`**: default GUC values set before every search sweep
- **`[search.sweep.params]`**: per-sweep-point GUCs (override defaults)

GUC keys are full PG names (e.g., `"mkt.nprobe"`, `"hnsw.ef_search"`),
validated before the sweep starts via `LOAD` + `SET`.

## Execution Model

### Full Run (rekall run)

When both index and search phases are requested, they interleave:

1. **Build** each index that doesn't already exist (skip existing)
2. For each index with sweeps:
   a. **Disable** all other rekall indexes (pg_index.indisvalid = false)
   b. **Enable** the active index
   c. Set default GUCs from `[index.gucs]`
   d. For each sweep parameter combination:
      - Set sweep GUCs (override defaults)
      - Warmup queries
      - Timed runs (best-of-N)
      - Compute recall vs ground truth
   e. Collect results
3. **Re-enable** all indexes
4. **Report**: Pareto frontier, results table, JSON export

Use `--rebuild` to force dropping and rebuilding all indexes.

### Run Traceability

Every run captures:
- Full effective config (base + local overrides)
- PG version, key settings (shared_buffers, work_mem, etc.)
- Extension versions + library version / git commit if available

Included in stdout output header and JSON under `config` / `pg_runtime`.

## Error UX

Commands validate preconditions and give actionable guidance:
- Missing table → "Run `rekall init` first"
- Empty table → "Run `rekall load` first"
- Missing index → "Run `rekall index` to build it"
- Bad GUC name → "Cannot set GUC 'mkt.nprob': check parameter name"
- Missing extension → "Install pgvector before running"

## Implementation Roadmap

### Phase 1: Core Pipeline (complete)

- Cargo workspace, config parsing, error types
- Streaming HDF5 reader with bounded-memory COPY BINARY loading
- Meerkat + pgvector HNSW backends with prepared statements
- Search with parameter sweeps, recall/QPS/latency percentiles
- Pareto frontier, stdout + JSON output
- Directory-based config groups, local overrides
- Index reuse via enable/disable, `--rebuild` flag
- Pre-commit hooks (fmt, clippy -D warnings, check, tests)
- 51 unit tests + 7 PG integration tests

### Phase 2: Scale

1. Parallel COPY (partition dataset, N streams)
2. Resumable loading (_rekall_load_progress table)
3. VectorChord backend
4. S3 dataset support (object_store crate)
5. Index build progress monitoring

### Phase 3: Agent + Results DB

1. PostgresSink (result storage schema)
2. Result commands (list, show, compare, export)
3. HTTP agent (axum + SSE progress)
4. Agent daemon mode

### Phase 4: Cloud Infrastructure

Terraform for machine provisioning, rekall agent for PG lifecycle:

1. Terraform modules for AWS (EC2 + EBS + PG install)
2. Agent commands: pg restart, reconfig, reinit, install-ext
3. CLI: `rekall infra up/down/status/ssh`

### Phase 5: Advanced

1. Ground truth computation (brute-force)
2. Concurrent query benchmarks (multi-client)
3. Plotting (gnuplot or plotters)
4. Predefined benchmark suites
