use std::time::Instant;

use async_trait::async_trait;
use byteorder::{BigEndian, WriteBytesExt};
use postgres_types::Type;
use tokio_postgres::{Client, NoTls, Statement};
use tracing::{debug, info};

use crate::config::{ConnectionConfig, Metric};
use crate::error::{Error, Result};
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
}

impl PgConnection {
    pub fn new(
        conn: &ConnectionConfig,
        table_name: &str,
        dimension: u32,
        metric: Metric,
    ) -> Self {
        Self {
            dsn: conn.dsn.clone(),
            table_name: table_name.to_string(),
            dimension,
            metric,
            client: None,
            query_stmt: None,
        }
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
    pub async fn connect(&mut self, order_operator: &str) -> Result<()> {
        let client = self.open_connection().await?;

        let vec_type = lookup_vector_type(&client).await.map_err(|e| {
            Error::Config(format!(
                "Cannot find pgvector 'vector' type: {e}"
            ))
        })?;

        let sql = format!(
            "SELECT id FROM {} ORDER BY v {} $1 LIMIT $2",
            self.table_name, order_operator,
        );
        let stmt = client
            .prepare_typed(&sql, &[vec_type, Type::INT8])
            .await
            .map_err(|e| {
                Error::Config(format!("Cannot prepare query: {e}"))
            })?;

        self.client = Some(client);
        self.query_stmt = Some(stmt);
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

    pub fn index_sql_name(&self, index_name: &str) -> String {
        format!("idx_rekall_{}", index_name.replace('-', "_"))
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
            .query_one(
                &format!("SELECT count(*) FROM {}", self.table_name),
                &[],
            )
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
