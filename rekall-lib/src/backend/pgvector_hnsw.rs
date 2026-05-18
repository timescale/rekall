use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use tokio_postgres::Client;
use tracing::info;

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::Result;
use crate::metrics::IndexBuildResult;

use super::postgres::PgConnection;
use super::IndexBackend;

#[derive(Debug, Deserialize)]
struct HnswParams {
    #[serde(default = "default_m")]
    m: u32,
    #[serde(default = "default_ef_construction")]
    ef_construction: u32,
}

fn default_m() -> u32 {
    16
}
fn default_ef_construction() -> u32 {
    200
}

pub struct PgvectorHnswBackend {
    pg: PgConnection,
    client: Client,
    name: String,
    params: HnswParams,
}

impl PgvectorHnswBackend {
    pub async fn create(
        conn: &ConnectionConfig,
        table: &str,
        dim: u32,
        metric: Metric,
        idx: &IndexConfig,
    ) -> Result<Self> {
        let params: HnswParams =
            idx.params.clone().try_into().map_err(|e| {
                crate::error::Error::Config(format!(
                    "Invalid pgvector-hnsw params for '{}': {e}",
                    idx.name
                ))
            })?;

        let pg = PgConnection::new(conn, table, dim, metric);
        let client = pg.connect().await?;

        Ok(Self {
            pg,
            client,
            name: idx.name.clone(),
            params,
        })
    }

    fn opclass(&self) -> &str {
        match self.pg.metric {
            Metric::Angular => "vector_cosine_ops",
            Metric::Euclidean => "vector_l2_ops",
        }
    }

    fn order_operator(&self) -> &str {
        match self.pg.metric {
            Metric::Angular => "<=>",
            Metric::Euclidean => "<->",
        }
    }
}

#[async_trait]
impl IndexBackend for PgvectorHnswBackend {
    fn type_name(&self) -> &str {
        "pgvector-hnsw"
    }

    fn index_name(&self) -> &str {
        &self.name
    }

    async fn build_index(&self) -> Result<IndexBuildResult> {
        let idx_name = self.pg.index_sql_name(&self.name);

        let sql = format!(
            "CREATE INDEX {idx_name} ON {} USING hnsw (v {}) \
             WITH (m = {}, ef_construction = {})",
            self.pg.table_name,
            self.opclass(),
            self.params.m,
            self.params.ef_construction,
        );

        info!("Building pgvector HNSW index: {sql}");
        let start = Instant::now();
        self.client.execute(&sql, &[]).await?;
        let build_time = start.elapsed();

        let size = self.index_size().await?;

        Ok(IndexBuildResult {
            index_name: self.name.clone(),
            index_type: "pgvector-hnsw".to_string(),
            params: serde_json::json!({
                "m": self.params.m,
                "ef_construction": self.params.ef_construction,
            }),
            build_time,
            index_size_bytes: size,
        })
    }

    async fn drop_index(&self) -> Result<()> {
        let idx_name = self.pg.index_sql_name(&self.name);
        self.client
            .execute(&format!("DROP INDEX IF EXISTS {idx_name}"), &[])
            .await?;
        Ok(())
    }

    async fn index_size(&self) -> Result<u64> {
        let idx_name = self.pg.index_sql_name(&self.name);
        let row = self
            .client
            .query_one(
                &format!("SELECT pg_relation_size('{idx_name}')"),
                &[],
            )
            .await?;
        let size: i64 = row.get(0);
        Ok(size as u64)
    }

    async fn index_exists(&self) -> Result<bool> {
        let idx_name = self.pg.index_sql_name(&self.name);
        let row = self
            .client
            .query_one(
                "SELECT EXISTS(
                    SELECT 1 FROM pg_indexes WHERE indexname = $1
                )",
                &[&idx_name],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn set_search_params(
        &self,
        params: &HashMap<String, toml::Value>,
    ) -> Result<()> {
        if let Some(ef) = params.get("ef_search") {
            let val = match ef {
                toml::Value::Integer(i) => i.to_string(),
                toml::Value::Float(f) => f.to_string(),
                _ => ef.to_string(),
            };
            self.client
                .execute(&format!("SET hnsw.ef_search = {val}"), &[])
                .await?;
        }

        Ok(())
    }

    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>> {
        let vec_lit = format!(
            "'[{}]'::vector",
            vector
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );

        let sql = format!(
            "SELECT id FROM {} ORDER BY v {} {} LIMIT {}",
            self.pg.table_name,
            self.order_operator(),
            vec_lit,
            k,
        );

        let rows = self.client.query(&sql, &[]).await?;
        Ok(rows.iter().map(|r| r.get::<_, i64>(0)).collect())
    }
}
