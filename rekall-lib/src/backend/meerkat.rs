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
        options_from_params(&self.params)
    }

    fn build_params_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.params).unwrap_or_default()
    }
}

/// Translate parsed params into the `CREATE INDEX ... WITH (...)` list.
/// Booleans are emitted only when `Some(true)`; floats only when `> 0.0`.
/// Kept as a free function so it can be unit-tested without standing up
/// a `PgConnection`.
fn options_from_params(params: &MeerkatParams) -> Vec<(&'static str, String)> {
    let mut opts = Vec::new();
    if let Some(nlist) = params.nlist {
        opts.push(("nlist", nlist.to_string()));
    }
    if let Some(fan_out) = params.fan_out {
        opts.push(("fan_out", fan_out.to_string()));
    }
    if params.centroid_compression == Some(true) {
        opts.push(("centroid_compression", "true".to_string()));
    }
    if params.rerank_vectors == Some(true) {
        opts.push(("rerank_vectors", "true".to_string()));
    }
    if params.fastscan == Some(true) {
        opts.push(("fastscan", "true".to_string()));
    }
    if let Some(eps) = params.boundary_epsilon {
        if eps > 0.0 {
            opts.push(("boundary_epsilon", eps.to_string()));
        }
    }
    if let Some(lambda) = params.soar_lambda {
        if lambda > 0.0 {
            opts.push(("soar_lambda", lambda.to_string()));
        }
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_params() -> MeerkatParams {
        MeerkatParams {
            nlist: None,
            fan_out: None,
            centroid_compression: None,
            rerank_vectors: None,
            fastscan: None,
            fastscan_bits: None,
            boundary_epsilon: None,
            soar_lambda: None,
        }
    }

    #[test]
    fn options_empty_when_all_defaults() {
        assert!(options_from_params(&default_params()).is_empty());
    }

    #[test]
    fn options_emit_numeric_params() {
        let mut p = default_params();
        p.nlist = Some(4000);
        p.fan_out = Some(16);
        let opts = options_from_params(&p);
        assert!(opts.contains(&("nlist", "4000".to_string())));
        assert!(opts.contains(&("fan_out", "16".to_string())));
    }

    #[test]
    fn options_skip_bool_flags_when_false() {
        let mut p = default_params();
        p.centroid_compression = Some(false);
        p.rerank_vectors = Some(false);
        p.fastscan = Some(false);
        // Bools only fire when Some(true) — Some(false) is a no-op.
        assert!(options_from_params(&p).is_empty());
    }

    #[test]
    fn options_emit_bool_flags_when_true() {
        let mut p = default_params();
        p.centroid_compression = Some(true);
        p.fastscan = Some(true);
        let opts = options_from_params(&p);
        assert!(opts.contains(&("centroid_compression", "true".to_string())));
        assert!(opts.contains(&("fastscan", "true".to_string())));
    }

    #[test]
    fn options_skip_floats_at_or_below_zero() {
        let mut p = default_params();
        p.boundary_epsilon = Some(0.0);
        p.soar_lambda = Some(-1.0);
        assert!(options_from_params(&p).is_empty());
    }

    #[test]
    fn options_emit_positive_floats() {
        let mut p = default_params();
        p.boundary_epsilon = Some(0.5);
        p.soar_lambda = Some(2.0);
        let opts = options_from_params(&p);
        assert!(opts.iter().any(|(k, v)| *k == "boundary_epsilon" && v == "0.5"));
        assert!(opts.iter().any(|(k, v)| *k == "soar_lambda" && v == "2"));
    }
}
