use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use byteorder::{BigEndian, WriteBytesExt};
use postgres_types::Type;
use tokio_postgres::{CancelToken, Client, NoTls, Statement};
use tracing::{debug, info};

use crate::config::{ConnectionConfig, Metric};
use crate::error::{Error, Result};
use crate::metrics::{ExtensionInfo, PgRuntimeInfo};
use crate::progress::{ProgressEvent, ProgressTx};

use super::pg_vector_type::lookup_vector_type;
use super::DataLoader;

/// Shared PostgreSQL connection state for all PG-based index backends.
/// Holds a persistent client, vector type info, and a prepared query
/// statement so that concrete backends don't duplicate this setup.
pub struct PgConnection {
    dsn: String,
    pub table_name: String,
    pub dimension: u32,
    pub metric: Metric,
    client: Option<Client>,
    pub query_stmt: Option<Statement>,
    order_operator: Option<String>,
    cancel_registry: Option<crate::CancelRegistry>,
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
        }
    }

    /// Set the cancel registry so connections register their tokens.
    pub fn set_cancel_registry(&mut self, registry: crate::CancelRegistry) {
        self.cancel_registry = Some(registry);
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

    /// Connect and prepare a query statement for the given order operator.
    /// Call this once during backend creation; then use client() and
    /// query_stmt() for all subsequent operations.
    pub async fn connect(&mut self, order_operator: &str, extension: Option<&str>) -> Result<()> {
        let client = self.open_connection().await?;

        if let Some(ext) = extension {
            client
                .execute(&format!("LOAD '{ext}'"), &[])
                .await
                .map_err(|_| Error::ExtensionMissing {
                    extension: ext.to_string(),
                })?;
        }

        let vec_type = lookup_vector_type(&client)
            .await
            .map_err(|e| Error::Config(format!("Cannot find pgvector 'vector' type: {e}")))?;

        let sql = format!(
            "SELECT id FROM {} ORDER BY v {} $1 LIMIT $2",
            self.table_name, order_operator,
        );
        let stmt = client
            .prepare_typed(&sql, &[vec_type, Type::INT8])
            .await
            .map_err(|e| Error::Config(format!("Cannot prepare query: {e}")))?;

        // Register cancel token for Ctrl+C handling
        if let Some(ref registry) = self.cancel_registry {
            let token = client.cancel_token();
            let registry = registry.clone();
            tokio::spawn(async move {
                registry.lock().await.push(token);
            });
        }

        self.client = Some(client);
        self.query_stmt = Some(stmt);
        self.order_operator = Some(order_operator.to_string());
        Ok(())
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
                     'max_connections', 'huge_pages'\
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

    /// Prewarm a relation into shared buffers using pg_prewarm.
    /// Limits prewarming to avoid evicting earlier pages when the relation
    /// is larger than shared_buffers. Prioritizes low-numbered blocks
    /// (centroids, metadata) which are accessed on every query.
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
        let size_mb = blocks as f64 * 8192.0 / (1024.0 * 1024.0);
        tracing::info!("Prewarmed '{relation}': {blocks}/{rel_blocks} blocks ({size_mb:.1} MB)");
        Ok(())
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
        format!(
            "idx_rekall_{}_{}",
            self.table_name,
            index_name.replace('-', "_")
        )
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
fn encode_copy_row(buf: &mut Vec<u8>, id: i64, vector: &[f32]) {
    buf.write_i16::<BigEndian>(2).unwrap(); // 2 fields

    // Field 1: id (int8)
    buf.write_i32::<BigEndian>(8).unwrap();
    buf.write_i64::<BigEndian>(id).unwrap();

    // Field 2: vector (pgvector binary format)
    // pgvector binary: 2-byte dim + 2-byte unused + dim * 4-byte floats
    let vec_len = 2 + 2 + vector.len() * 4;
    buf.write_i32::<BigEndian>(vec_len as i32).unwrap();
    buf.write_u16::<BigEndian>(vector.len() as u16).unwrap();
    buf.write_u16::<BigEndian>(0).unwrap(); // unused, must be 0
    for &val in vector {
        buf.write_f32::<BigEndian>(val).unwrap();
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
                    "CREATE TABLE {} (\
                        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                        v vector({}))",
                    self.table_name, self.dimension
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
                encode_copy_row(&mut buf, id, row.as_slice().unwrap());
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
        encode_copy_row(&mut buf, 42, &[1.0, 2.0, 3.0]);
        // First 2 bytes: field count = 2
        assert_eq!(&buf[..2], &[0, 2]);
    }

    #[test]
    fn encode_row_id_field() {
        let mut buf = Vec::new();
        encode_copy_row(&mut buf, 12345, &[1.0]);
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
        encode_copy_row(&mut buf, 0, &vec);

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
        encode_copy_row(&mut buf, 0, &vector);
        let expected = 2 + (4 + 8) + (4 + 2 + 2 + dim * 4);
        assert_eq!(buf.len(), expected);
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
