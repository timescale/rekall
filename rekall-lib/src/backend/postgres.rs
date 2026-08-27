use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use byteorder::{BigEndian, WriteBytesExt};
use postgres_types::Type;
use tokio_postgres::{CancelToken, Client, NoTls, Statement};
use tracing::{debug, info};

use crate::config::{ConnectionConfig, Metric, StorageKind};
use crate::error::{Error, Result};
use crate::metrics::{ExtensionInfo, PgRuntimeInfo};
use crate::progress::{ProgressEvent, ProgressTx};

use super::DataLoader;

/// Shared PostgreSQL connection state for all PG-based index backends.
/// Holds a persistent client, vector type info, and a prepared query
/// statement so that concrete backends don't duplicate this setup.
pub struct PgConnection {
    pub(crate) dsn: String,
    pub table_name: String,
    pub dimension: u32,
    pub metric: Metric,
    client: Option<Client>,
    pub query_stmt: Option<Statement>,
    order_operator: Option<String>,
    cancel_registry: Option<crate::CancelRegistry>,
    pub(crate) progress: Option<ProgressTx>,
    /// GUCs applied on the build session before CREATE INDEX (e.g.
    /// `max_parallel_maintenance_workers`). Distinct from search GUCs.
    pub(crate) build_gucs: HashMap<String, toml::Value>,
    /// Heap storage for the vector column. Defaults to `Vector`; the
    /// phases that do DDL or COPY set it from the config, since only they
    /// need it -- a read-only probe does not care.
    pub storage: StorageKind,
    /// When set, this arm reuses another config's physical index:
    /// `index_sql_name` resolves to the referenced config's index so one
    /// built index can be swept under several query-time configurations.
    pub(crate) physical_index: Option<String>,
}

impl PgConnection {
    pub fn new(conn: &ConnectionConfig, table_name: &str, dimension: u32, metric: Metric) -> Self {
        Self {
            dsn: conn.dsn.clone(),
            table_name: table_name.to_string(),
            dimension,
            metric,
            client: None,
            query_stmt: None,
            order_operator: None,
            cancel_registry: None,
            progress: None,
            build_gucs: HashMap::new(),
            storage: StorageKind::default(),
            physical_index: None,
        }
    }

    /// Set the heap storage kind for DDL and COPY encoding.
    pub fn set_storage(&mut self, storage: StorageKind) {
        self.storage = storage;
    }

    /// Point this arm at another config's physical index (see
    /// `IndexConfig::physical_index`).
    pub fn set_physical_index(&mut self, physical: Option<String>) {
        self.physical_index = physical;
    }

    /// Set the cancel registry so connections register their tokens.
    pub fn set_cancel_registry(&mut self, registry: crate::CancelRegistry) {
        self.cancel_registry = Some(registry);
    }

    /// Set GUCs to apply on the build session before CREATE INDEX.
    pub fn set_build_gucs(&mut self, gucs: HashMap<String, toml::Value>) {
        self.build_gucs = gucs;
    }

    /// Apply the configured build GUCs to the current session. No-op
    /// when none are set.
    pub async fn apply_build_gucs(&self) -> Result<()> {
        if self.build_gucs.is_empty() {
            return Ok(());
        }
        self.set_gucs(&self.build_gucs).await
    }

    /// Set the progress channel for build monitoring.
    pub fn set_progress(&mut self, tx: ProgressTx) {
        self.progress = Some(tx);
    }

    /// Open a new connection (used for loading and DDL before connect() is called).
    pub async fn open_connection(&self) -> Result<Client> {
        let (client, connection) =
            tokio_postgres::connect(&self.dsn, NoTls)
                .await
                .map_err(|e| Error::PgConnect {
                    dsn: self.dsn.clone(),
                    source: e,
                })?;

        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!("PG connection error: {e}");
            }
        });

        Ok(client)
    }

    /// Open the underlying client without any extension setup or
    /// statement preparation. Used for read-only probes that just
    /// need a session (e.g. `runtime_info()`).
    pub async fn connect(&mut self) -> Result<()> {
        self.connect_with_setup(&[]).await
    }

    /// Open the client, then `CREATE EXTENSION IF NOT EXISTS` for each
    /// entry in dependency order and `LOAD` the last (the primary
    /// extension whose operators are needed at query time). Does NOT
    /// prepare the search statement — that's done by the IndexBackend
    /// trait's `connect()` default, which has access to the backend's
    /// query column/bind hooks.
    pub async fn connect_with_setup(&mut self, extensions: &[&str]) -> Result<()> {
        let client = self.open_connection().await?;

        for ext in extensions {
            client
                .execute(&format!("CREATE EXTENSION IF NOT EXISTS {ext}"), &[])
                .await
                .map_err(|e| {
                    tracing::error!("CREATE EXTENSION {ext} failed: {e}");
                    Error::ExtensionMissing {
                        extension: (*ext).to_string(),
                    }
                })?;
        }

        if let Some(ext) = extensions.last() {
            // Resolve the shared library this extension's C functions are
            // actually bound to instead of assuming it matches the bare
            // extension name: some extensions (e.g. meerkat) install a
            // version-suffixed .so for side-by-side installs, so
            // `LOAD '{ext}'` can silently load a stale/different file
            // sharing the unversioned name and double-init the real one
            // when it's loaded again via the access method handler.
            let probin_query = format!(
                "SELECT DISTINCT probin FROM pg_proc p \
                 JOIN pg_depend d ON d.objid = p.oid \
                 JOIN pg_extension e ON e.oid = d.refobjid \
                 WHERE e.extname = '{ext}' AND probin IS NOT NULL LIMIT 1"
            );
            let libpath: String = match client.query_opt(&probin_query, &[]).await {
                Ok(Some(row)) => row.get(0),
                _ => format!("$libdir/{ext}"),
            };
            client
                .execute(&format!("LOAD '{libpath}'"), &[])
                .await
                .map_err(|e| {
                    tracing::error!("LOAD '{libpath}' failed: {e}");
                    Error::ExtensionMissing {
                        extension: (*ext).to_string(),
                    }
                })?;
        }

        // Register cancel token for Ctrl+C handling
        if let Some(ref registry) = self.cancel_registry {
            let token = client.cancel_token();
            let registry = registry.clone();
            tokio::spawn(async move {
                registry.lock().await.push(token);
            });
        }

        self.client = Some(client);
        Ok(())
    }

    /// Install a prepared search statement and its order operator.
    /// Called by the IndexBackend trait's default `connect()` after
    /// the connection is open and the statement is prepared.
    pub fn install_query_stmt(&mut self, stmt: Statement, order_operator: &str) {
        self.query_stmt = Some(stmt);
        self.order_operator = Some(order_operator.to_string());
    }

    pub fn client(&self) -> &Client {
        self.client
            .as_ref()
            .expect("PgConnection::connect() not called")
    }

    pub fn query_stmt(&self) -> &Statement {
        self.query_stmt
            .as_ref()
            .expect("PgConnection::connect() not called")
    }

    /// Get a cancel token for the current connection. Send this to a
    /// signal handler to cancel in-flight PG queries on Ctrl+C.
    pub fn cancel_token(&self) -> Option<CancelToken> {
        self.client.as_ref().map(|c| c.cancel_token())
    }

    /// Validate that all GUC names in the param map exist.
    /// Call this once before starting a sweep to fail fast on typos.
    /// Validate GUC names. PG allows setting arbitrary dotted-name GUCs
    /// (custom GUCs from extensions), so we validate by attempting a SET
    /// with the first value in each sweep. A non-dotted name that doesn't
    /// exist will error; dotted names always succeed as custom GUCs.
    /// We warn if a dotted name isn't a registered GUC (might be a typo).
    pub async fn validate_gucs(&self, params: &HashMap<String, Vec<toml::Value>>) -> Result<()> {
        let client = self.client();
        for (key, values) in params {
            if values.is_empty() {
                return Err(Error::Config(format!("Sweep param '{key}' has no values")));
            }

            let test_val = toml_value_to_sql(&values[0]);
            if let Err(e) = client
                .execute(&format!("SET \"{key}\" = {test_val}"), &[])
                .await
            {
                return Err(Error::Config(format!(
                    "Cannot set GUC '{key}' = {test_val}: {e}. \
                     Check that the parameter name is correct."
                )));
            }

            // For dotted names (custom GUCs), check if it's actually
            // registered by looking in pg_settings. Warn if not found
            // (might be a typo like mkt.nprob instead of mkt.nprobe).
            if key.contains('.') {
                let row = client
                    .query_one(
                        "SELECT EXISTS(
                            SELECT 1 FROM pg_settings WHERE name = $1
                        )",
                        &[key],
                    )
                    .await?;
                let exists: bool = row.get(0);
                if !exists {
                    tracing::warn!(
                        "GUC '{key}' is not a registered parameter \
                         (not in pg_settings). It will be treated as a \
                         custom GUC. If this is a typo, check the \
                         parameter name."
                    );
                }
            }
        }
        Ok(())
    }

    /// Set GUCs from a param map. Keys are GUC names (e.g., "mkt.nprobe"),
    /// values are the settings. Issues `SET key = value` for each entry.
    pub async fn set_gucs(&self, params: &HashMap<String, toml::Value>) -> Result<()> {
        let client = self.client();
        for (key, value) in params {
            let val_str = toml_value_to_sql(value);
            client
                .execute(&format!("SET \"{key}\" = {val_str}"), &[])
                .await
                .map_err(|e| Error::Config(format!("Failed to SET {key} = {val_str}: {e}")))?;
        }
        Ok(())
    }

    /// Capture PG runtime environment: version, key settings, extensions.
    pub async fn runtime_info(&self) -> Result<PgRuntimeInfo> {
        let client = self.client();

        let version: String = client.query_one("SELECT version()", &[]).await?.get(0);

        let rows = client
            .query(
                "SELECT name, setting, unit FROM pg_settings \
                 WHERE name IN (\
                     'shared_buffers', 'work_mem', 'effective_cache_size', \
                     'maintenance_work_mem', 'max_parallel_workers', \
                     'max_parallel_maintenance_workers', 'random_page_cost', \
                     'effective_io_concurrency', 'wal_level', \
                     'max_connections', 'huge_pages', \
                     'data_directory', 'cluster_name', 'server_version', \
                     'maintenance_io_concurrency', 'checkpoint_completion_target', \
                     'max_wal_size', 'jit'\
                 ) ORDER BY name",
                &[],
            )
            .await?;

        let mut settings = std::collections::BTreeMap::new();
        for row in &rows {
            let name: String = row.get(0);
            let setting: String = row.get(1);
            let unit: Option<String> = row.get(2);
            let value = match unit {
                Some(u) if !u.is_empty() => {
                    // Convert to human-readable: e.g., 524288 * 8kB = 4GB
                    if let Ok(num) = setting.parse::<i64>() {
                        let bytes = match u.as_str() {
                            "kB" => num * 1024,
                            "8kB" => num * 8 * 1024,
                            "MB" => num * 1024 * 1024,
                            "s" | "ms" | "min" => {
                                // Time units: just format with unit
                                settings.insert(name, format!("{setting} {u}"));
                                continue;
                            }
                            _ => {
                                settings.insert(name, format!("{setting} {u}"));
                                continue;
                            }
                        };
                        if bytes >= 1024 * 1024 * 1024 {
                            format!("{} GB", bytes / (1024 * 1024 * 1024))
                        } else if bytes >= 1024 * 1024 {
                            format!("{} MB", bytes / (1024 * 1024))
                        } else {
                            format!("{} kB", bytes / 1024)
                        }
                    } else {
                        format!("{setting} {u}")
                    }
                }
                _ => setting,
            };
            settings.insert(name, value);
        }

        let ext_rows = client
            .query(
                "SELECT extname, extversion FROM pg_extension \
                 WHERE extname != 'plpgsql' ORDER BY extname",
                &[],
            )
            .await?;

        let mut extensions = std::collections::BTreeMap::new();
        for row in &ext_rows {
            let name: String = row.get(0);
            let ver: String = row.get(1);

            // Probe for version/commit functions that extensions may provide.
            // Try common patterns: <ext>.version(), <ext>.git_commit(),
            // <ext>_version(), <ext>_git_commit()
            let schema = match name.as_str() {
                "meerkat" => Some("mkt"),
                _ => None,
            };

            let version_candidates = match schema {
                Some(s) => vec![
                    // meerkat >= 0.1.0 release machinery
                    format!("SELECT {s}.extension_version()"),
                    // older meerkat dev builds
                    format!("SELECT {s}.version()"),
                    format!("SELECT {s}_version()"),
                    format!("SELECT {name}_version()"),
                ],
                None => vec![format!("SELECT {name}_version()")],
            };

            let commit_candidates = match schema {
                Some(s) => vec![
                    format!("SELECT {s}.git_commit()"),
                    format!("SELECT {s}_git_commit()"),
                    format!("SELECT {name}_git_commit()"),
                ],
                None => vec![format!("SELECT {name}_git_commit()")],
            };

            let library_version = try_query_string(client, &version_candidates).await;
            let git_commit = try_query_string(client, &commit_candidates).await;

            extensions.insert(
                name,
                ExtensionInfo {
                    version: ver,
                    git_commit,
                    library_version,
                },
            );
        }

        Ok(PgRuntimeInfo {
            version,
            settings,
            extensions,
        })
    }

    /// Run EXPLAIN on the query with a sample vector and verify the plan
    /// uses the expected index. Returns an error (stopping the benchmark)
    /// if a Seq Scan or wrong index is detected.
    pub async fn validate_query_plan(
        &self,
        expected_index: &str,
        sample_vector: &[f32],
        k: usize,
    ) -> Result<String> {
        let client = self.client();
        let order_op = self
            .order_operator
            .as_deref()
            .expect("PgConnection::connect() not called");

        let vec_lit = format!(
            "'[{}]'::vector",
            sample_vector
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );

        let explain_sql = format!(
            "EXPLAIN (FORMAT TEXT) SELECT id FROM {} ORDER BY v {} {} LIMIT {}",
            self.table_name, order_op, vec_lit, k,
        );

        let rows = client
            .query(&explain_sql, &[])
            .await
            .map_err(|e| Error::Config(format!("EXPLAIN failed: {e}")))?;

        let plan_text = rows
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n");

        let expected_idx_sql = self.index_sql_name(expected_index);

        if !plan_text.contains(&expected_idx_sql) {
            if plan_text.contains("Seq Scan") {
                return Err(Error::Config(format!(
                    "Query plan uses Seq Scan instead of index \
                     '{expected_idx_sql}'. The index may not exist, is \
                     disabled, or the operator doesn't match.\n\
                     Plan:\n{plan_text}"
                )));
            }
            return Err(Error::Config(format!(
                "Query plan does not use expected index '{expected_idx_sql}'. \
                 Another index may be active.\n\
                 Plan:\n{plan_text}"
            )));
        }

        tracing::debug!("Query plan validated: using {expected_idx_sql}");
        Ok(plan_text)
    }

    /// Prewarm a relation into the OS page cache only, blocking until every
    /// page is resident ('read' mode). Use for the heap: its pages are
    /// fetched at most a few dozen times per query (rerank), which the OS
    /// page cache serves fine — pushing a heap prefix into shared buffers
    /// instead EVICTS the index pages every query depends on (measured: a
    /// 144 GB heap-prefix buffer warm degraded warm-state queries ~5x by
    /// flushing the index out of shared buffers). Without any heap pass, a
    /// heap bigger than RAM-resident state costs 15-25% QPS on a cold page
    /// cache.
    pub async fn prewarm_oscache(&self, relation: &str) -> Result<()> {
        let client = self.client();
        client
            .execute("CREATE EXTENSION IF NOT EXISTS pg_prewarm", &[])
            .await?;
        let start = std::time::Instant::now();
        let row = client
            .query_one(&format!("SELECT pg_prewarm('{relation}', 'read')"), &[])
            .await?;
        let blocks: i64 = row.get(0);
        tracing::info!(
            "Prewarmed '{relation}': {blocks} blocks ({:.1} MB) read into \
             the OS page cache in {:.1}s (blocking; shared buffers untouched)",
            blocks as f64 * 8192.0 / (1024.0 * 1024.0),
            start.elapsed().as_secs_f64(),
        );
        Ok(())
    }

    /// Prewarm a relation into shared buffers (75% budget, low-numbered
    /// blocks first — centroids/metadata that every query touches), reading
    /// any remainder into the OS page cache, blocking until resident. Use
    /// for indexes: their pages are hit on every query and deserve buffer
    /// residency.
    pub async fn prewarm(&self, relation: &str) -> Result<()> {
        let client = self.client();
        client
            .execute("CREATE EXTENSION IF NOT EXISTS pg_prewarm", &[])
            .await?;

        // Get relation size in blocks and shared_buffers size
        let rel_row = client
            .query_one(
                &format!("SELECT pg_relation_size('{relation}') / 8192"),
                &[],
            )
            .await?;
        let rel_blocks: i64 = rel_row.get(0);
        if rel_blocks == 0 {
            tracing::info!("Prewarm '{relation}': empty relation, skipped");
            return Ok(());
        }

        let buf_row = client
            .query_one(
                "SELECT setting::bigint FROM pg_settings \
                 WHERE name = 'shared_buffers'",
                &[],
            )
            .await?;
        let shared_buffers_blocks: i64 = buf_row.get(0);

        // Leave headroom: use at most 75% of shared_buffers for this relation
        let max_blocks = (shared_buffers_blocks * 3 / 4).min(rel_blocks);
        let last_block = max_blocks - 1;

        let row = client
            .query_one(
                &format!("SELECT pg_prewarm('{relation}', 'buffer', 'main', 0, {last_block})"),
                &[],
            )
            .await?;
        let blocks: i64 = row.get(0);

        // Remainder past the shared-buffers budget: OS page cache only,
        // read synchronously so the caller can rely on the pages being
        // resident when this returns.
        let mut prefetched: i64 = 0;
        let remainder_start = std::time::Instant::now();
        if rel_blocks > max_blocks {
            let row = client
                .query_one(
                    &format!(
                        "SELECT pg_prewarm('{relation}', 'read', 'main', \
                         {max_blocks}, {})",
                        rel_blocks - 1
                    ),
                    &[],
                )
                .await?;
            prefetched = row.get(0);
        }
        let remainder_secs = remainder_start.elapsed().as_secs_f64();

        let size_mb = blocks as f64 * 8192.0 / (1024.0 * 1024.0);
        let prefetch_mb = prefetched as f64 * 8192.0 / (1024.0 * 1024.0);
        tracing::info!(
            "Prewarmed '{relation}': {blocks}/{rel_blocks} blocks \
             ({size_mb:.1} MB) in shared buffers, {prefetched} blocks \
             ({prefetch_mb:.1} MB) read into page cache in {remainder_secs:.1}s \
             (blocking)"
        );
        Ok(())
    }

    /// Force an immediate checkpoint, blocking until it completes. CHECKPOINT
    /// is synchronous: when it returns, every shared buffer dirty at its
    /// start has been written and fsynced, and files written directly by
    /// backends since the last checkpoint (bulk-write ring buffers during
    /// index builds) have been fsynced too. Run it in untimed setup regions
    /// so a timed phase doesn't compete with the previous phase's background
    /// writeback. Needs superuser or the pg_checkpoint predefined role.
    /// Returns the elapsed seconds.
    pub async fn checkpoint(&self) -> Result<f64> {
        let start = std::time::Instant::now();
        self.client().execute("CHECKPOINT", &[]).await?;
        Ok(start.elapsed().as_secs_f64())
    }

    /// Evict all shared buffers for a relation using pg_buffercache.
    pub async fn evict_relation_buffers(&self, relation: &str) -> Result<()> {
        let client = self.client();
        client
            .execute("CREATE EXTENSION IF NOT EXISTS pg_buffercache", &[])
            .await?;

        // Try pg_buffercache_evict_relation (pg_buffercache 1.5+)
        let result = client
            .execute(
                &format!("SELECT pg_buffercache_evict_relation('{relation}'::regclass)"),
                &[],
            )
            .await;

        if result.is_ok() {
            tracing::info!("Evicted shared buffers for '{relation}'");
        } else {
            tracing::warn!(
                "pg_buffercache_evict_relation not available for '{relation}'. \
                 For true cold runs, restart PostgreSQL."
            );
        }
        Ok(())
    }

    /// Disable an index so the planner won't use it.
    pub async fn disable_index(&self, index_sql_name: &str) -> Result<()> {
        let client = self.client();
        client
            .execute(
                &format!(
                    "UPDATE pg_index SET indisvalid = false \
                     WHERE indexrelid = '{index_sql_name}'::regclass"
                ),
                &[],
            )
            .await?;
        tracing::debug!("Disabled index {index_sql_name}");
        Ok(())
    }

    /// Re-enable a previously disabled index.
    pub async fn enable_index(&self, index_sql_name: &str) -> Result<()> {
        let client = self.client();
        client
            .execute(
                &format!(
                    "UPDATE pg_index SET indisvalid = true \
                     WHERE indexrelid = '{index_sql_name}'::regclass"
                ),
                &[],
            )
            .await?;
        tracing::debug!("Enabled index {index_sql_name}");
        Ok(())
    }

    /// Add a derived column to the table and populate it from `v`.
    /// Used by backends that index a quantized/cast representation
    /// (halfvec, bit, rabitq{4,8}, ...). Idempotent: `ADD COLUMN IF
    /// NOT EXISTS` + `UPDATE WHERE col IS NULL`, so re-runs are cheap.
    ///
    /// `type_decl` is the full PG type with its modifier — e.g.
    /// `"halfvec(768)"`, `"bit(768)"`, `"rabitq8(768)"`.
    /// `from_v_expr` is the SQL expression that produces a value of
    /// `type_decl` from the existing `v` column — e.g.
    /// `"v::halfvec(768)"`, `"binary_quantize(v)::bit(768)"`,
    /// `"quantize_to_rabitq8(v)"`.
    pub async fn ensure_derived_column(
        &self,
        column: &str,
        type_decl: &str,
        from_v_expr: &str,
    ) -> Result<()> {
        let client = self.client();
        let table = &self.table_name;
        client
            .execute(
                &format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {column} {type_decl}"),
                &[],
            )
            .await?;
        client
            .execute(
                &format!("UPDATE {table} SET {column} = {from_v_expr} WHERE {column} IS NULL"),
                &[],
            )
            .await?;
        Ok(())
    }

    /// Drop a column previously added by `ensure_derived_column`.
    /// Called from a backend's `drop_index` so sweeps stay idempotent.
    pub async fn drop_derived_column(&self, column: &str) -> Result<()> {
        self.client()
            .execute(
                &format!(
                    "ALTER TABLE {} DROP COLUMN IF EXISTS {column}",
                    self.table_name
                ),
                &[],
            )
            .await?;
        Ok(())
    }

    /// Prepare a search statement whose first parameter is a `vector`
    /// (matching pgvector's binary wire format via `PgVector`). `$2` is
    /// always `INT8` (the LIMIT). Additional parameter types can be
    /// passed for queries with extra placeholders (e.g. a candidate
    /// pool size for rerank queries).
    pub async fn prepare_vector_search(
        &self,
        sql: &str,
        extra_param_types: &[Type],
    ) -> Result<Statement> {
        let vec_type = super::pg_vector_type::lookup_vector_type(self.client())
            .await
            .map_err(|e| Error::Config(format!("Cannot find pgvector 'vector' type: {e}")))?;
        let mut types: Vec<Type> = vec![vec_type, Type::INT8];
        types.extend_from_slice(extra_param_types);
        self.client()
            .prepare_typed(sql, &types)
            .await
            .map_err(|e| Error::Config(format!("Cannot prepare search query: {e}\n  SQL: {sql}")))
    }

    /// Get SQL names of all rekall-managed indexes on this table.
    pub async fn list_rekall_indexes(&self) -> Result<Vec<String>> {
        let client = self.client();
        let rows = client
            .query(
                "SELECT indexname FROM pg_indexes \
                 WHERE tablename = $1 AND indexname LIKE 'idx_rekall_%'",
                &[&self.table_name],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
    }

    pub fn index_sql_name(&self, index_name: &str) -> String {
        // A physical_index arm keeps its own name in results and plots
        // but resolves every SQL-level reference to the shared index.
        let name = self.physical_index.as_deref().unwrap_or(index_name);
        format!("idx_rekall_{}_{}", self.table_name, name.replace('-', "_"))
    }
}

async fn try_query_string(client: &Client, candidates: &[String]) -> Option<String> {
    for sql in candidates {
        if let Ok(row) = client.query_one(sql.as_str(), &[]).await {
            if let Ok(val) = row.try_get::<_, String>(0) {
                return Some(val);
            }
        }
    }
    None
}

fn toml_value_to_sql(v: &toml::Value) -> String {
    match v {
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::String(s) => format!("'{s}'"),
        _ => v.to_string(),
    }
}

/// Encode a single row in COPY BINARY format.
/// Format: field_count (i16) + for each field: len (i32) + data
/// Encode one COPY BINARY row for the column's actual type.
///
/// COPY BINARY does not apply casts -- the bytes have to already be in the
/// target type's `recv` format -- so pgvector's `vector::halfvec` cast is no
/// help here and the loader has to emit f16 itself. `half::f16::from_f32`
/// rounds to nearest, ties to even, which is what pgvector's own input path
/// does; `halfvec_copy_matches_cast` in the tests below pins that agreement
/// against a live server rather than assuming it.
fn encode_copy_row(buf: &mut Vec<u8>, id: i64, vector: &[f32], storage: StorageKind) {
    buf.write_i16::<BigEndian>(2).unwrap(); // 2 fields

    // Field 1: id (int8)
    buf.write_i32::<BigEndian>(8).unwrap();
    buf.write_i64::<BigEndian>(id).unwrap();

    // Field 2: the vector. Both pgvector types share the same header --
    // 2-byte dim + 2-byte unused -- and differ only in element width.
    let elem_bytes = match storage {
        StorageKind::Vector => 4,
        StorageKind::Halfvec => 2,
    };
    let vec_len = 2 + 2 + vector.len() * elem_bytes;
    buf.write_i32::<BigEndian>(vec_len as i32).unwrap();
    buf.write_u16::<BigEndian>(vector.len() as u16).unwrap();
    buf.write_u16::<BigEndian>(0).unwrap(); // unused, must be 0
    match storage {
        StorageKind::Vector => {
            for &val in vector {
                buf.write_f32::<BigEndian>(val).unwrap();
            }
        }
        StorageKind::Halfvec => {
            for &val in vector {
                buf.write_u16::<BigEndian>(half::f16::from_f32(val).to_bits())
                    .unwrap();
            }
        }
    }
}

fn copy_header() -> Vec<u8> {
    let mut buf = Vec::with_capacity(19);
    // PGCOPY\n\xff\r\n\0
    buf.extend_from_slice(b"PGCOPY\n\xff\r\n\0");
    buf.write_i32::<BigEndian>(0).unwrap(); // flags
    buf.write_i32::<BigEndian>(0).unwrap(); // header extension
    buf
}

fn copy_trailer() -> Vec<u8> {
    let mut buf = Vec::with_capacity(2);
    buf.write_i16::<BigEndian>(-1).unwrap();
    buf
}

#[async_trait]
impl DataLoader for PgConnection {
    async fn table_exists(&self) -> Result<bool> {
        let client = self.open_connection().await?;
        let row = client
            .query_one(
                "SELECT EXISTS(
                    SELECT 1 FROM information_schema.tables
                    WHERE table_name = $1
                )",
                &[&self.table_name],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn row_count(&self) -> Result<u64> {
        let client = self.open_connection().await?;
        let row = client
            .query_one(&format!("SELECT count(*) FROM {}", self.table_name), &[])
            .await?;
        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    async fn create_table(&self) -> Result<()> {
        let client = self.open_connection().await?;

        client
            .execute("CREATE EXTENSION IF NOT EXISTS vector", &[])
            .await
            .map_err(|e| {
                if e.to_string().contains("could not open extension") {
                    Error::ExtensionMissing {
                        extension: "vector".to_string(),
                    }
                } else {
                    e.into()
                }
            })?;

        client
            .execute(
                &format!("DROP TABLE IF EXISTS {} CASCADE", self.table_name),
                &[],
            )
            .await?;

        client
            .execute(
                &format!(
                    // No PRIMARY KEY on `id`, and no IDENTITY.
                    //
                    // Nothing ever fetches a row by id: the loader writes it
                    // with `COPY (id, v)` and searches only ever SELECT it,
                    // to match results against ground-truth indices. So the
                    // btree was pure overhead: 21 MB per million rows of
                    // page cache nothing reads -- 2.1 GB at cohere-100M,
                    // which is real on a memory-constrained box.
                    //
                    // It also cost write work during the bulk load. A
                    // controlled INSERT..SELECT of 1M rows measured 29.0s
                    // with the index maintained against 13.4s without, for
                    // an index that takes 0.5s to build post-hoc. That
                    // delta does NOT show up end-to-end in `rekall load`
                    // at 1M, where the COPY is bound by HDF5 read
                    // throughput and the VACUUM (FREEZE, ANALYZE) that
                    // follows dominates the clock -- but it is write work
                    // that buys nothing, and it grows with the table.
                    //
                    // Uniqueness is guaranteed by construction anyway: ids
                    // come from a monotonic counter (chunk.start_index + i).
                    // The IDENTITY went with it -- the loader always supplies
                    // id, so its sequence never advanced past its initial
                    // value and would have collided immediately had anything
                    // ever inserted without one.
                    "CREATE TABLE {} (\
                        id bigint NOT NULL, \
                        v {})",
                    self.table_name,
                    self.storage.sql_type(self.dimension)
                ),
                &[],
            )
            .await?;

        client
            .execute(
                &format!(
                    "ALTER TABLE {} ALTER COLUMN v SET STORAGE PLAIN",
                    self.table_name
                ),
                &[],
            )
            .await?;

        info!("Created table {} (dim={})", self.table_name, self.dimension);
        Ok(())
    }

    async fn create_meta_tables(&self) -> Result<()> {
        let client = self.open_connection().await?;

        client
            .execute(
                "CREATE TABLE IF NOT EXISTS _rekall_meta (
                    key text PRIMARY KEY,
                    value jsonb NOT NULL
                )",
                &[],
            )
            .await?;

        client
            .execute(
                "CREATE TABLE IF NOT EXISTS _rekall_load_progress (
                    range_start bigint NOT NULL,
                    range_end bigint NOT NULL,
                    loaded_at timestamptz NOT NULL DEFAULT now(),
                    PRIMARY KEY (range_start, range_end)
                )",
                &[],
            )
            .await?;

        debug!("Created metadata tables");
        Ok(())
    }

    async fn load_data(
        &self,
        total: u64,
        chunk_rx: &mut tokio::sync::mpsc::Receiver<Result<crate::dataset::VectorChunk>>,
        progress: &ProgressTx,
    ) -> Result<u64> {
        let client = self.open_connection().await?;

        let copy_stmt = format!(
            "COPY {} (id, v) FROM STDIN WITH (FORMAT BINARY)",
            self.table_name
        );

        let sink = client.copy_in(&copy_stmt).await?;

        use futures_util::SinkExt;
        let mut sink = std::pin::pin!(sink);

        sink.as_mut()
            .send(bytes::Bytes::from(copy_header()))
            .await
            .map_err(Error::Pg)?;

        let row_byte_size = 2 + 4 + 8 + 4 + 2 + 2 + self.dimension as usize * 4;
        let mut buf = Vec::with_capacity(50_000 * row_byte_size);
        let mut loaded: u64 = 0;
        let start_time = Instant::now();

        while let Some(chunk_result) = chunk_rx.recv().await {
            let chunk = chunk_result?;
            let n_rows = chunk.vectors.nrows();

            buf.clear();
            for i in 0..n_rows {
                let id = (chunk.start_index + i as u64) as i64;
                let row = chunk.vectors.row(i);
                encode_copy_row(&mut buf, id, row.as_slice().unwrap(), self.storage);
            }

            sink.as_mut()
                .send(bytes::Bytes::copy_from_slice(&buf))
                .await
                .map_err(Error::Pg)?;

            loaded += n_rows as u64;

            let elapsed = start_time.elapsed().as_secs_f64();
            let rate = if elapsed > 0.0 {
                loaded as f64 / elapsed
            } else {
                0.0
            };
            let _ = progress.send(ProgressEvent::LoadProgress {
                loaded,
                total,
                rows_per_sec: rate,
            });
        }

        sink.as_mut()
            .send(bytes::Bytes::from(copy_trailer()))
            .await
            .map_err(Error::Pg)?;

        sink.close().await.map_err(Error::Pg)?;

        info!("Loaded {loaded} rows into {}", self.table_name);

        client
            .execute(&format!("ANALYZE {}", self.table_name), &[])
            .await?;

        Ok(loaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_header_format() {
        let hdr = copy_header();
        assert_eq!(&hdr[..11], b"PGCOPY\n\xff\r\n\0");
        assert_eq!(hdr.len(), 19); // signature(11) + flags(4) + ext(4)
    }

    #[test]
    fn copy_trailer_format() {
        let trailer = copy_trailer();
        assert_eq!(trailer, vec![0xFF, 0xFF]); // -1 as i16 big-endian
    }

    #[test]
    fn encode_row_field_count() {
        let mut buf = Vec::new();
        encode_copy_row(&mut buf, 42, &[1.0, 2.0, 3.0], StorageKind::Vector);
        // First 2 bytes: field count = 2
        assert_eq!(&buf[..2], &[0, 2]);
    }

    #[test]
    fn encode_row_id_field() {
        let mut buf = Vec::new();
        encode_copy_row(&mut buf, 12345, &[1.0], StorageKind::Vector);
        // After field_count(2): id field len(4) = 8, then id(8)
        let id_len = i32::from_be_bytes(buf[2..6].try_into().unwrap());
        assert_eq!(id_len, 8);
        let id = i64::from_be_bytes(buf[6..14].try_into().unwrap());
        assert_eq!(id, 12345);
    }

    #[test]
    fn encode_row_vector_field() {
        let vec = [1.5_f32, -2.5, 3.0];
        let mut buf = Vec::new();
        encode_copy_row(&mut buf, 0, &vec, StorageKind::Vector);

        // Vector field starts at offset 14 (2+4+8)
        let vec_offset = 14;
        let vec_len = i32::from_be_bytes(buf[vec_offset..vec_offset + 4].try_into().unwrap());
        assert_eq!(vec_len, 2 + 2 + 3 * 4); // dim + unused + 3 floats = 16

        let dim = u16::from_be_bytes(buf[vec_offset + 4..vec_offset + 6].try_into().unwrap());
        assert_eq!(dim, 3);

        let unused = u16::from_be_bytes(buf[vec_offset + 6..vec_offset + 8].try_into().unwrap());
        assert_eq!(unused, 0);

        for (i, &expected) in vec.iter().enumerate() {
            let off = vec_offset + 8 + i * 4;
            let val = f32::from_be_bytes(buf[off..off + 4].try_into().unwrap());
            assert_eq!(val, expected);
        }
    }

    #[test]
    fn encode_row_total_size() {
        let dim = 768;
        let vector = vec![0.0_f32; dim];
        let mut buf = Vec::new();
        encode_copy_row(&mut buf, 0, &vector, StorageKind::Vector);
        let expected = 2 + (4 + 8) + (4 + 2 + 2 + dim * 4);
        assert_eq!(buf.len(), expected);
    }

    #[test]
    fn halfvec_encoding_matches_pgvector_rounding() {
        // COPY BINARY applies no casts, so the loader has to do the f32->f16
        // conversion itself and its rounding must match what pgvector's own
        // `vector::halfvec` cast produces -- otherwise a halfvec table loaded
        // by rekall would hold different values than the same data inserted
        // through SQL, and recall would differ for reasons nothing records.
        //
        // The expected values were read off a live pgvector 0.8.6:
        //   SELECT ('[...]'::vector(10)::halfvec(10))::text
        // They agree bit-for-bit with half::f16::from_f32, including the
        // ties-to-even case (2049 -> 2048) and a near-subnormal (6.1e-5).
        // The literals are exactly what was typed into psql to obtain the
        // expected column, so they must not be shortened -- 1.0009765625 is
        // 1 + 2^-10, the smallest value above 1.0 that f16 holds exactly,
        // which is the point of including it.
        #[allow(clippy::excessive_precision)]
        let input: [f32; 10] = [
            0.255478,
            -0.280852,
            0.305302,
            0.501733,
            0.109043,
            1.0009765625,
            2049.0,
            6.1e-5,
            0.1,
            0.3333333,
        ];
        let pgvector: [f32; 10] = [
            0.2553711,
            -0.28076172,
            0.30541992,
            0.5019531,
            0.109069824,
            1.0009766,
            2048.0,
            6.097555e-05,
            0.099975586,
            0.33325195,
        ];
        for (i, (&x, &want)) in input.iter().zip(pgvector.iter()).enumerate() {
            let got = f32::from(half::f16::from_f32(x));
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "element {i}: {x} -> {got}, pgvector gives {want}"
            );
        }
    }

    #[test]
    fn halfvec_copy_row_is_half_the_width() {
        // Same header, 2-byte elements instead of 4.
        let v = [1.0f32, 2.0, 3.0, 4.0];
        let mut full = Vec::new();
        encode_copy_row(&mut full, 1, &v, StorageKind::Vector);
        let mut half_ = Vec::new();
        encode_copy_row(&mut half_, 1, &v, StorageKind::Halfvec);
        assert_eq!(full.len() - half_.len(), v.len() * 2);
    }

    #[test]
    fn toml_value_to_sql_types() {
        assert_eq!(toml_value_to_sql(&toml::Value::Integer(42)), "42");
        #[allow(clippy::approx_constant)]
        {
            assert_eq!(toml_value_to_sql(&toml::Value::Float(3.14)), "3.14");
        }
        assert_eq!(toml_value_to_sql(&toml::Value::Boolean(true)), "true");
        assert_eq!(
            toml_value_to_sql(&toml::Value::String("hello".into())),
            "'hello'"
        );
    }

    #[test]
    fn index_sql_name_sanitizes() {
        let pg = PgConnection::new(
            &ConnectionConfig { dsn: String::new() },
            "t",
            1,
            Metric::Angular,
        );
        assert_eq!(
            pg.index_sql_name("mkt-nlist-1000"),
            "idx_rekall_t_mkt_nlist_1000"
        );
        assert_eq!(pg.index_sql_name("hnsw-m16"), "idx_rekall_t_hnsw_m16");
    }
}
