use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::Result;
use crate::metrics::IndexBuildResult;

use super::pg_vector_type::PgVector;
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
            crate::error::Error::Config(format!("Invalid meerkat params for '{}': {e}", idx.name))
        })?;

        let order_op = match metric {
            Metric::Angular => "OPERATOR(mkt.<=>)",
            Metric::Euclidean => "OPERATOR(mkt.<->)",
        };

        let mut pg = PgConnection::new(conn, table, dim, metric);
        pg.connect(order_op, Some("meerkat")).await?;

        Ok(Self {
            pg,
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
        let client = self.pg.client();

        client
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
        client.execute(&sql, &[]).await?;
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
        self.pg
            .client()
            .execute(&format!("DROP INDEX IF EXISTS {idx_name}"), &[])
            .await?;
        Ok(())
    }

    async fn index_size(&self) -> Result<u64> {
        let idx_name = self.pg.index_sql_name(&self.name);
        let row = self
            .pg
            .client()
            .query_one(&format!("SELECT pg_relation_size('{idx_name}')"), &[])
            .await?;
        let size: i64 = row.get(0);
        Ok(size as u64)
    }

    async fn index_exists(&self) -> Result<bool> {
        let idx_name = self.pg.index_sql_name(&self.name);
        let row = self
            .pg
            .client()
            .query_one(
                "SELECT EXISTS(
                    SELECT 1 FROM pg_indexes WHERE indexname = $1
                )",
                &[&idx_name],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn set_search_params(&self, params: &HashMap<String, toml::Value>) -> Result<()> {
        self.pg.set_gucs(params).await
    }

    async fn validate_search_params(
        &self,
        params: &HashMap<String, Vec<toml::Value>>,
    ) -> Result<()> {
        self.pg.validate_gucs(params).await
    }

    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>> {
        let k_i64 = k as i64;
        let rows = self
            .pg
            .client()
            .query(self.pg.query_stmt(), &[&PgVector(vector), &k_i64])
            .await?;
        Ok(rows.iter().map(|r| r.get::<_, i64>(0)).collect())
    }
}
