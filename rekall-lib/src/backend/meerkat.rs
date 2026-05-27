use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::Result;
use crate::metrics::IndexBuildResult;

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
        cancel_registry: Option<&crate::CancelRegistry>,
    ) -> Result<Self> {
        let params: MeerkatParams = idx.params.clone().try_into().map_err(|e| {
            crate::error::Error::Config(format!("Invalid meerkat params for '{}': {e}", idx.name))
        })?;

        let order_op = match metric {
            Metric::Angular => "OPERATOR(mkt.<=>)",
            Metric::Euclidean => "OPERATOR(mkt.<->)",
        };

        let mut pg = PgConnection::new(conn, table, dim, metric);
        if let Some(cr) = cancel_registry {
            pg.set_cancel_registry(cr.clone());
        }
        pg.connect_with_setup(
            order_op,
            Some("meerkat"),
            &["CREATE EXTENSION IF NOT EXISTS meerkat"],
        )
        .await?;

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

    fn pg(&self) -> &PgConnection {
        &self.pg
    }
}
