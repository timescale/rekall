use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use postgres_types::Type;
use serde::{Deserialize, Serialize};
use tokio_postgres::{Client, Statement};
use tracing::info;

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::Result;
use crate::metrics::IndexBuildResult;

use super::pg_vector_type::{lookup_vector_type, PgVector};
use super::postgres::PgConnection;
use super::IndexBackend;

#[derive(Debug, Serialize, Deserialize)]
struct MeerkatParams {
    #[serde(default)]
    nlist: Option<u32>,
    #[serde(default)]
    fan_out: Option<u32>,
    #[serde(default)]
    centroid_compression: Option<bool>,
    #[serde(default)]
    rerank_vectors: Option<bool>,
    #[serde(default)]
    fastscan: Option<bool>,
    #[serde(default)]
    fastscan_bits: Option<u32>,
    #[serde(default)]
    boundary_epsilon: Option<f64>,
    #[serde(default)]
    soar_lambda: Option<f64>,
}

pub struct MeerkatBackend {
    pg: PgConnection,
    client: Client,
    query_stmt: Statement,
    name: String,
    params: MeerkatParams,
}

impl MeerkatBackend {
    pub async fn create(
        conn: &ConnectionConfig,
        table: &str,
        dim: u32,
        metric: Metric,
        idx: &IndexConfig,
    ) -> Result<Self> {
        let params: MeerkatParams = idx.params.clone().try_into().map_err(|e| {
            crate::error::Error::Config(format!(
                "Invalid meerkat params for '{}': {e}",
                idx.name
            ))
        })?;

        let pg = PgConnection::new(conn, table, dim, metric);
        let client = pg.connect().await?;

        let vec_type = lookup_vector_type(&client).await.map_err(|e| {
            crate::error::Error::Config(format!(
                "Cannot find pgvector 'vector' type: {e}"
            ))
        })?;

        let order_op = match metric {
            Metric::Angular => "OPERATOR(mkt.<=>)",
            Metric::Euclidean => "OPERATOR(mkt.<->)",
        };

        let sql = format!(
            "SELECT id FROM {table} ORDER BY v {order_op} $1 LIMIT $2"
        );
        let query_stmt = client
            .prepare_typed(&sql, &[vec_type, Type::INT8])
            .await
            .map_err(|e| {
                crate::error::Error::Config(format!(
                    "Cannot prepare meerkat query: {e}"
                ))
            })?;

        Ok(Self {
            pg,
            client,
            query_stmt,
            name: idx.name.clone(),
            params,
        })
    }

    fn opclass(&self) -> &str {
        match self.pg.metric {
            Metric::Angular => "mkt.vector_cosine_ops",
            Metric::Euclidean => "mkt.vector_l2_ops",
        }
    }

    fn with_clause(&self) -> String {
        let mut opts = Vec::new();

        if let Some(nlist) = self.params.nlist {
            opts.push(format!("nlist = {nlist}"));
        }
        if let Some(fan_out) = self.params.fan_out {
            opts.push(format!("fan_out = {fan_out}"));
        }
        if self.params.centroid_compression == Some(true) {
            opts.push("centroid_compression = true".to_string());
        }
        if self.params.rerank_vectors == Some(true) {
            opts.push("rerank_vectors = true".to_string());
        }
        if self.params.fastscan == Some(true) {
            opts.push("fastscan = true".to_string());
        }
        if let Some(eps) = self.params.boundary_epsilon {
            if eps > 0.0 {
                opts.push(format!("boundary_epsilon = {eps}"));
            }
        }
        if let Some(lambda) = self.params.soar_lambda {
            if lambda > 0.0 {
                opts.push(format!("soar_lambda = {lambda}"));
            }
        }

        if opts.is_empty() {
            String::new()
        } else {
            format!(" WITH ({})", opts.join(", "))
        }
    }
}

#[async_trait]
impl IndexBackend for MeerkatBackend {
    fn type_name(&self) -> &str {
        "meerkat"
    }

    fn index_name(&self) -> &str {
        &self.name
    }

    async fn build_index(&self) -> Result<IndexBuildResult> {
        self.client
            .execute("CREATE EXTENSION IF NOT EXISTS meerkat", &[])
            .await
            .map_err(|e| {
                if e.to_string().contains("could not open extension") {
                    crate::error::Error::ExtensionMissing {
                        extension: "meerkat".to_string(),
                    }
                } else {
                    e.into()
                }
            })?;

        let idx_name = self.pg.index_sql_name(&self.name);
        let sql = format!(
            "CREATE INDEX {idx_name} ON {} USING mktann (v {}){}",
            self.pg.table_name,
            self.opclass(),
            self.with_clause(),
        );

        info!("Building meerkat index: {sql}");
        let start = Instant::now();
        self.client.execute(&sql, &[]).await?;
        let build_time = start.elapsed();

        let size = self.index_size().await?;

        Ok(IndexBuildResult {
            index_name: self.name.clone(),
            index_type: "meerkat".to_string(),
            params: serde_json::to_value(&self.params).unwrap_or_default(),
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
        if let Some(nprobe) = params.get("nprobe") {
            self.client
                .execute(
                    &format!("SET mkt.nprobe = {}", toml_to_sql(nprobe)),
                    &[],
                )
                .await?;
        }
        if let Some(bits) = params.get("fastscan_bits") {
            self.client
                .execute(
                    &format!(
                        "SET mkt.fastscan_bits = {}",
                        toml_to_sql(bits)
                    ),
                    &[],
                )
                .await?;
        }

        Ok(())
    }

    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>> {
        let k_i64 = k as i64;
        let rows = self
            .client
            .query(&self.query_stmt, &[&PgVector(vector), &k_i64])
            .await?;
        Ok(rows.iter().map(|r| r.get::<_, i64>(0)).collect())
    }
}

fn toml_to_sql(v: &toml::Value) -> String {
    match v {
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::String(s) => format!("'{s}'"),
        _ => v.to_string(),
    }
}
