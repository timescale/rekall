use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::Result;

use super::postgres::PgConnection;
use super::{pg_backend_field_accessors, IndexBackend};

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
        super::create_pg_backend(
            conn,
            table,
            dim,
            metric,
            idx,
            cancel_registry,
            "meerkat",
            |pg, name, params| Self { pg, name, params },
        )
        .await
    }
}

#[async_trait]
impl IndexBackend for MeerkatBackend {
    pg_backend_field_accessors!();

    fn type_name(&self) -> &str {
        "meerkat"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["meerkat"]
    }

    fn order_op(&self) -> &'static str {
        match self.pg.metric {
            Metric::Angular => "OPERATOR(mkt.<=>)",
            Metric::Euclidean => "OPERATOR(mkt.<->)",
        }
    }

    fn index_method(&self) -> &str {
        "mktann"
    }

    fn opclass(&self) -> &str {
        match self.pg.metric {
            Metric::Angular => "mkt.vector_cosine_ops",
            Metric::Euclidean => "mkt.vector_l2_ops",
        }
    }

    fn index_options(&self) -> Vec<(&'static str, String)> {
        let mut opts = Vec::new();
        if let Some(nlist) = self.params.nlist {
            opts.push(("nlist", nlist.to_string()));
        }
        if let Some(fan_out) = self.params.fan_out {
            opts.push(("fan_out", fan_out.to_string()));
        }
        if self.params.centroid_compression == Some(true) {
            opts.push(("centroid_compression", "true".to_string()));
        }
        if self.params.rerank_vectors == Some(true) {
            opts.push(("rerank_vectors", "true".to_string()));
        }
        if self.params.fastscan == Some(true) {
            opts.push(("fastscan", "true".to_string()));
        }
        if let Some(eps) = self.params.boundary_epsilon {
            if eps > 0.0 {
                opts.push(("boundary_epsilon", eps.to_string()));
            }
        }
        if let Some(lambda) = self.params.soar_lambda {
            if lambda > 0.0 {
                opts.push(("soar_lambda", lambda.to_string()));
            }
        }
        opts
    }

    fn build_params_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.params).unwrap_or_default()
    }
}
