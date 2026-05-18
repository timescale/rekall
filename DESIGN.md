# Rekall Design Document

## Overview

Rekall is a Rust-based vector search benchmark tool designed to scale from 1M
to 100M+ vectors. It operates in independent phases (init, load, index, search,
report) so that expensive operations like dataset loading aren't repeated
unnecessarily. Multiple index types can share the same loaded data.

## Architecture

```
                    +-----------+
                    |  CLI      |  rekall run/init/load/index/search/report
                    +-----+-----+
                          |
                    +-----v-----+
                    |   Agent   |  Phase orchestrator, progress events
                    +-----+-----+
                          |
          +---------------+---------------+
          |               |               |
    +-----v-----+  +-----v-----+  +------v-----+
    | DatasetRdr|  | IndexBack |  | OutputSink |
    | (HDF5)    |  | (trait)   |  | (trait)    |
    +-----------+  +-----+-----+  +------------+
                         |
              +----------+----------+
              |                     |
        +-----v------+     +-------v--------+
        | PgConnection|     | RemoteBackend  |
        | (shared)   |     | (future)       |
        +-----+------+     +----------------+
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
      lib.rs
      config.rs                 # TOML benchmark config parsing
      error.rs                  # thiserror error enum
      dataset/
        mod.rs                  # DatasetReader trait
        hdf5.rs                 # Chunked HDF5 streaming reader
      backend/
        mod.rs                  # IndexBackend trait + registry
        postgres.rs             # Shared PG: connect, COPY BINARY, table mgmt
        pgvector_hnsw.rs        # pgvector HNSW provider
        meerkat.rs              # Meerkat mktann provider
      phase/
        mod.rs
        init.rs                 # Create DB, tables, metadata schema
        load.rs                 # Streaming parallel COPY, resumable
        index.rs                # Build/drop indexes
        search.rs               # Query benchmarks with parameter sweeps
        report.rs               # Aggregate results, Pareto frontier
      output/
        mod.rs                  # OutputSink trait
        stdout.rs               # Formatted tables to terminal
        json.rs                 # JSON file export
      metrics.rs                # Recall, QPS, latency percentiles
      progress.rs               # Progress event channel
      infra/                    # Cloud provisioning (Phase 4)
        mod.rs                  # InfraBackend trait
        aws.rs                  # AWS EC2 provisioning
  rekall/                       # CLI + agent binary
    Cargo.toml
    src/
      main.rs                   # clap CLI with subcommands
      agent.rs                  # Agent: phase orchestrator, progress logging
```

Two crates: a library and a single binary. The agent runs in the same binary
as the CLI -- in Phase 1 it's a foreground orchestrator that logs progress to
stdout. The daemon/remote aspects come later.

## Core Abstractions

### IndexBackend

The central trait. Each implementation knows how to build an index, run queries,
and report metrics for a specific vector index type.

```rust
#[async_trait]
pub trait IndexBackend: Send + Sync {
    fn type_name(&self) -> &str;
    async fn init(&self, dataset: &DatasetConfig) -> Result<()>;
    async fn load(&self, reader: &mut dyn DatasetReader, progress: &ProgressTx) -> Result<u64>;
    async fn load_progress(&self) -> Result<LoadProgress>;
    async fn build_index(&self, params: &IndexParams) -> Result<IndexBuildResult>;
    async fn drop_index(&self, name: &str) -> Result<()>;
    async fn index_size(&self, name: &str) -> Result<u64>;
    async fn query(&self, vector: &[f32], k: usize, params: &SearchParams) -> Result<Vec<i64>>;
    async fn set_search_params(&self, params: &SearchParams) -> Result<()>;
    async fn cleanup(&self) -> Result<()>;
}
```

### PgConnection (Composition Over Inheritance)

PostgreSQL backends share a `PgConnection` struct that handles connection
pooling, table creation, and COPY BINARY loading. Concrete backends
(MeerkatIndexBackend, PgvectorHnswIndexBackend) own a PgConnection and
delegate common operations to it. Index DDL, query SQL, and GUC settings
are backend-specific.

```rust
pub struct PgConnection {
    pool: deadpool_postgres::Pool,
    table_name: String,
    dimension: u32,
    metric: Metric,
}
```

Multiple PG backends share the SAME loaded table. Load once, build different
indexes.

### DatasetReader (Streaming HDF5)

```rust
pub trait DatasetReader: Send {
    fn meta(&self) -> &DatasetMeta;
    fn train_chunks(&mut self, chunk_size: usize)
        -> Box<dyn Iterator<Item = Result<VectorChunk>> + '_>;
    fn train_range(&mut self, start: u64, end: u64, chunk_size: usize)
        -> Box<dyn Iterator<Item = Result<VectorChunk>> + '_>;
    fn queries(&mut self) -> Result<Array2<f32>>;
    fn ground_truth(&mut self) -> Result<Array2<i64>>;
}
```

`train_chunks` and `train_range` read HDF5 via hyperslab selection -- one
chunk at a time into a reusable buffer. The full dataset is never loaded
into memory.

### OutputSink (Pluggable Results)

```rust
pub trait OutputSink: Send {
    fn on_phase_start(&mut self, phase: &str, detail: &str);
    fn on_progress(&mut self, progress: &ProgressEvent);
    fn on_index_built(&mut self, result: &IndexBuildResult);
    fn on_search_result(&mut self, result: &SearchResult);
    fn on_pareto(&mut self, frontier: &[ParetoPoint]);
    fn on_complete(&mut self, summary: &RunSummary);
}
```

Implementations: `StdoutSink` (formatted tables), `JsonSink` (JSON file).
Future: `PostgresSink` (results DB for historical comparison).

## Configuration

Benchmarks are defined in TOML config files. Each index backend receives
opaque `toml::Value` params and parses its own config struct, avoiding a
central enum that grows with every backend.

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
soar_lambda = 1.0
boundary_epsilon = 0.05

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

[output]
format = ["table", "json"]
json_path = "results/cohere-1m-results.json"
```

## CLI Commands

```
rekall run <config.toml>              # Full run (all phases)
  --phase <init|load|index|search|report>
  --index <name>                      # Only this index
  --resume                            # Resume interrupted load
  --dry-run
  -v, --verbose

rekall init <config.toml>             # Init DB/tables only
rekall load <config.toml>             # Load dataset only
  --resume
  --parallelism <N>
rekall index <config.toml>            # Build indexes only
  --name <name>
rekall search <config.toml>           # Run search only
  --index <name>
rekall report <config.toml>           # Generate report from last run

rekall dataset info <path.hdf5>       # Show HDF5 metadata
rekall dataset validate <path.hdf5>   # Validate structure

rekall config show                    # Show global defaults
rekall config init                    # Create default config
```

## Data Flow

### Phase Pipeline

```
init   ->  CREATE TABLE cohere_1m (id bigint GENERATED ALWAYS AS IDENTITY,
                                    v vector(768))
           CREATE TABLE _rekall_meta (key text PRIMARY KEY, value jsonb)
           CREATE TABLE _rekall_load_progress (range_start bigint,
                                               range_end bigint,
                                               loaded_at timestamptz)

load   ->  HDF5 chunked read -> N parallel COPY BINARY streams -> cohere_1m
           Track loaded ranges in _rekall_load_progress for resume

index  ->  For each [[index]] in config:
             DROP INDEX IF EXISTS idx_rekall_{name}
             CREATE INDEX idx_rekall_{name} ON cohere_1m USING {method} ...
             Record build_time, index_size

search ->  Load queries + ground_truth from HDF5 (small, fits in RAM)
           For each sweep config:
             SET backend-specific GUCs (nprobe, ef_search, etc.)
             Warmup queries
             For each run:
               Time each query individually
               Compute recall vs ground truth
             Aggregate: avg recall, QPS, p50/p95/p99 latency
           Compute Pareto frontier across all sweep points

report ->  Format results via OutputSink(s)
```

### Parallel COPY Loading

```
Dataset: 1,000,000 vectors, parallelism=4

Thread 0: rows      0 - 249,999  -> COPY BINARY -> PG
Thread 1: rows 250,000 - 499,999  -> COPY BINARY -> PG
Thread 2: rows 500,000 - 749,999  -> COPY BINARY -> PG
Thread 3: rows 750,000 - 999,999  -> COPY BINARY -> PG

Each thread:
  1. Open HDF5, seek to assigned range
  2. Open PG connection from pool
  3. BEGIN transaction
  4. COPY ... FROM STDIN WITH (FORMAT BINARY)
  5. Stream chunks, write binary rows (int4 id + vector bytes)
  6. On completion: INSERT INTO _rekall_load_progress
  7. COMMIT

Resume: query _rekall_load_progress, skip completed ranges.
```

### Memory-Bounded Loading

Loading must use bounded memory regardless of dataset size. The 100M Cohere
dataset is ~286 GB as float32; loading it must use the same ~600 MB of
working memory as loading 1M vectors.

**Constraints:**

- HDF5 reads via hyperslab selection: one chunk (10K-50K vectors) at a time
  into a reusable buffer. Never map or read the full dataset.
- COPY BINARY stream: encode each chunk and flush to the socket before reading
  the next. The chunk buffer is reused, not accumulated.
- Per-thread memory: ~chunk_size x dimension x 4 bytes + COPY encoding buffer.
  At chunk_size=50K, dim=768: ~150 MB per thread. With 4 threads: ~600 MB total.
- Backpressure: if PG can't consume COPY data fast enough, TCP socket backpressure
  blocks the writer naturally. No unbounded queuing.

**Speed optimizations:**

- COPY BINARY bypasses SQL parsing and type coercion
- Parallel streams scale linearly until disk/network bottleneck
- Large chunk sizes reduce HDF5 read overhead
- Pipeline: encode next batch while previous batch flushes
- No indexes during load; build after load completes
- ANALYZE after load for accurate planner statistics
- Consider `wal_level=minimal` + `COPY FREEZE` for fresh tables

## Agent Architecture

The agent is the orchestration layer that drives phases and emits progress:

```rust
pub struct Agent {
    config: BenchmarkConfig,
    backends: HashMap<String, Box<dyn IndexBackend>>,
    outputs: Vec<Box<dyn OutputSink>>,
}

impl Agent {
    pub async fn run(&mut self, phases: &[Phase]) -> Result<RunSummary> {
        for phase in phases {
            self.emit(ProgressEvent::PhaseStart { phase });
            match phase {
                Phase::Init => self.run_init().await?,
                Phase::Load => self.run_load().await?,
                Phase::Index => self.run_index().await?,
                Phase::Search => self.run_search().await?,
                Phase::Report => self.run_report().await?,
            }
            self.emit(ProgressEvent::PhaseEnd { phase });
        }
        // ...
    }
}
```

Phase 1: `rekall run config.toml` creates an Agent and calls `agent.run()`
in the foreground. Progress is logged to stdout via StdoutSink. Later, the
same Agent wraps in an HTTP server for remote control (Phase 3).

## Error UX

Commands validate preconditions and give actionable guidance:

- `rekall load` before init: "Table 'cohere_1m' does not exist. Run
  `rekall init <config.toml>` first."
- `rekall search` with no index: "No index 'mkt-nlist-1000' found. Run
  `rekall index <config.toml>` to build it."
- Wrong DSN: "Cannot connect to PostgreSQL at ...: connection refused.
  Check [connection].dsn in your config."
- Missing HDF5 file: "Dataset not found: /path/to/file.hdf5. Check
  [dataset].path in your config."
- Resume with nothing to resume: "Dataset already fully loaded
  (1,000,000 / 1,000,000 rows). Use --force to reload."
- Empty table: "Table 'cohere_1m' has 0 rows. Run `rekall load
  <config.toml>` first."
- Missing extension: "Extension 'vector' is not available. Install
  pgvector before running this benchmark."

Pattern: detect the missing precondition, explain what's wrong, and tell
the user exactly which command to run to fix it.

## Cloud Infrastructure (Phase 4)

Two-layer design for cloud benchmarking:

**Terraform** handles infrastructure lifecycle (create/destroy machines,
storage, networking). Runs infrequently. Declarative, version-controlled.

**Rekall agent** handles service lifecycle on provisioned machines
(start/stop/restart PG, reconfigure postgresql.conf, reinstall extensions,
reinitialize data directory). Runs frequently during benchmarking.

This separation means you can reconfigure/restart PG, change shared_buffers,
toggle shared_preload_libraries, or wipe the data directory without
reprovisioning the machine.

```
rekall/terraform/
  modules/
    aws-bench-instance/         # EC2 + EBS + security group + PG install
    aws-bench-network/          # VPC, subnet (optional, multi-node)
```

```toml
[infra]
provider = "aws"
region = "us-east-1"
instance_type = "r6g.4xlarge"
storage_gb = 500
pg_version = "18"
extensions = ["pgvector", "meerkat"]
```

## Implementation Roadmap

### Phase 1: Core Pipeline

Run `rekall run cohere-1m.toml` end-to-end with Meerkat + pgvector HNSW.

1. Scaffold Cargo workspace (two crates)
2. Config parsing (BenchmarkConfig with serde)
3. Error types (thiserror)
4. HDF5 chunked reader
5. PgConnection (connect, create table, single-stream COPY BINARY)
6. MeerkatIndexBackend + PgvectorHnswIndexBackend
7. IndexBackend registry
8. Phase orchestration (init, load, index, search)
9. Search benchmarking (warmup, timed queries, recall)
10. Metrics (recall@k, QPS, latency percentiles via hdrhistogram)
11. StdoutSink + JsonSink output
12. Agent struct (foreground orchestrator)
13. CLI (clap subcommands)

### Phase 2: Scale

Handle 100M vectors.

1. Parallel COPY (partition dataset, N streams)
2. Resumable loading (_rekall_load_progress table)
3. VectorChord backend
4. S3 dataset support (object_store crate)
5. Index build progress monitoring

### Phase 3: Agent + Results DB

Remote monitoring, persistent results.

1. PostgresSink (result storage schema)
2. Result commands (list, show, compare, export)
3. HTTP agent (axum + SSE progress)
4. Agent daemon mode

### Phase 4: Cloud Infrastructure

1. Terraform modules for AWS
2. Rekall agent PG management commands
3. CLI: `rekall infra up/down/status/ssh`

### Phase 5: Advanced

1. Ground truth computation (brute-force)
2. Concurrent query benchmarks (multi-client)
3. Plotting (gnuplot or plotters)
4. Predefined benchmark suites

## Dependencies

```toml
[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tokio-postgres = { version = "0.7", features = ["with-serde_json-1"] }
deadpool-postgres = "0.14"
hdf5 = "0.9"
chrono = { version = "0.4", features = ["serde"] }
indicatif = "0.17"
hdrhistogram = "7"
dirs = "5"
tabled = "0.17"
```
