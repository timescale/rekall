// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

//! pg_pinecone backend: the extension's native engine, an IVF index of
//! RaBitQ codes with exact rescoring from the heap.
//!
//! The index is always built with `mode = 'local'`, so it never needs a
//! Pinecone API key or network access. The remote engine is out of scope:
//! it runs the search in Pinecone's service, not in this PostgreSQL.
//!
//! A build calibrates the default probe count and rescore pool for
//! `target_recall`. Searches override both per session through
//! `pinecone.scan_fraction` (probed lists / `n_lists`) and
//! `pinecone.rescore_multiplier` (pool = value x 100 rows); `0` keeps the
//! calibrated value.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::Result;

use super::postgres::PgConnection;
use super::{pg_backend_field_accessors, IndexBackend};

/// Build-time reloptions of a native pg_pinecone index. `None` leaves the
/// extension's default, which is also its recommended setting.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PgPineconeParams {
    /// IVF list count. Default: `ceil(sqrt(rows))`.
    #[serde(default)]
    n_lists: Option<u32>,
    /// Pins the rescore pool to this x 100 rows. Default: calibrated.
    #[serde(default)]
    rescore_multiplier: Option<u32>,
    /// recall@10 the build calibrates the default probes and pool for
    /// (0.5 to 0.999). Default: 0.95.
    #[serde(default)]
    target_recall: Option<f64>,
    /// Row count below which the index is a flat exact scan. Default: 10000.
    #[serde(default)]
    small_n_threshold: Option<u32>,
    /// Seed of the k-means and the RaBitQ rotation. Default: derived from
    /// the index OID, so set it for reproducible builds.
    #[serde(default)]
    seed: Option<u32>,
    /// `schema.table` of real queries to calibrate on instead of table
    /// rows. Must not hold the evaluation queries.
    #[serde(default)]
    calibration_queries: Option<String>,
}

pub(crate) fn parse_params(value: &toml::Value) -> std::result::Result<(), toml::de::Error> {
    let _: PgPineconeParams = value.clone().try_into()?;
    Ok(())
}

pub struct PgPineconeBackend {
    pg: PgConnection,
    name: String,
    params: PgPineconeParams,
}

impl PgPineconeBackend {
    /// pg_pinecone's control file does not declare pgvector, so `vector`
    /// must be created first: the extension's opclasses are built over it.
    fn extension_names() -> &'static [&'static str] {
        &["vector", "pg_pinecone"]
    }

    fn opclass_for_metric(metric: Metric) -> &'static str {
        match metric {
            Metric::Angular => "vector_cosine_ops",
            Metric::Euclidean => "vector_l2_ops",
        }
    }

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
            "pg_pinecone",
            |pg, name, params| Self { pg, name, params },
        )
        .await
    }
}

#[async_trait]
impl IndexBackend for PgPineconeBackend {
    pg_backend_field_accessors!();

    fn type_name(&self) -> &str {
        "pg_pinecone"
    }

    fn type_names() -> &'static [&'static str] {
        &["pg_pinecone"]
    }

    fn validate_params(idx: &IndexConfig) -> Result<()> {
        parse_params(&idx.params).map_err(|e| super::invalid_params(idx, e))
    }

    fn extensions(&self) -> &'static [&'static str] {
        Self::extension_names()
    }

    fn order_op(&self) -> &'static str {
        match self.pg.metric {
            Metric::Angular => "<=>",
            Metric::Euclidean => "<->",
        }
    }

    fn index_method(&self) -> &str {
        "pinecone"
    }

    fn opclass(&self) -> &str {
        Self::opclass_for_metric(self.pg.metric)
    }

    fn index_options(&self) -> Vec<(&'static str, String)> {
        options_from_params(&self.params)
    }

    fn build_params_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.params).unwrap_or_default()
    }
}

/// Translate parsed params into the `CREATE INDEX ... WITH (...)` list.
/// `mode = 'local'` always comes first: the default `auto` would pick the
/// remote engine on a database that has a stored Pinecone API key.
fn options_from_params(params: &PgPineconeParams) -> Vec<(&'static str, String)> {
    let mut opts = vec![("mode", "'local'".to_string())];
    if let Some(n) = params.n_lists {
        opts.push(("n_lists", n.to_string()));
    }
    if let Some(rm) = params.rescore_multiplier {
        opts.push(("rescore_multiplier", rm.to_string()));
    }
    if let Some(t) = params.target_recall {
        opts.push(("target_recall", t.to_string()));
    }
    if let Some(n) = params.small_n_threshold {
        opts.push(("small_n_threshold", n.to_string()));
    }
    if let Some(seed) = params.seed {
        opts.push(("seed", seed.to_string()));
    }
    if let Some(ref q) = params.calibration_queries {
        opts.push(("calibration_queries", sql_string_literal(q)));
    }
    opts
}

fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: toml::Value) -> PgPineconeParams {
        value.try_into().unwrap()
    }

    #[test]
    fn defaults_emit_only_local_mode() {
        let opts = options_from_params(&PgPineconeParams::default());
        assert_eq!(opts, vec![("mode", "'local'".to_string())]);
    }

    #[test]
    fn all_params_are_emitted_after_mode() {
        let p = parse(
            toml::toml! {
                n_lists = 4000
                rescore_multiplier = 10
                target_recall = 0.99
                small_n_threshold = 5000
                seed = 42
                calibration_queries = "public.real_queries"
            }
            .into(),
        );
        assert_eq!(
            options_from_params(&p),
            vec![
                ("mode", "'local'".to_string()),
                ("n_lists", "4000".to_string()),
                ("rescore_multiplier", "10".to_string()),
                ("target_recall", "0.99".to_string()),
                ("small_n_threshold", "5000".to_string()),
                ("seed", "42".to_string()),
                ("calibration_queries", "'public.real_queries'".to_string()),
            ]
        );
    }

    #[test]
    fn calibration_queries_quotes_are_escaped() {
        let p = PgPineconeParams {
            calibration_queries: Some("public.o'brien".to_string()),
            ..Default::default()
        };
        assert!(options_from_params(&p)
            .contains(&("calibration_queries", "'public.o''brien'".to_string())));
    }

    #[test]
    fn unknown_and_ignored_params_are_rejected() {
        // k_bits is accepted by the extension but has no effect since 0.10;
        // rejecting it keeps a suite from believing it changed the build.
        for bad in ["k_bits", "mode", "nlist"] {
            let mut table = toml::value::Table::new();
            table.insert(bad.to_string(), toml::Value::Integer(1));
            let err = parse_params(&toml::Value::Table(table))
                .unwrap_err()
                .to_string();
            assert!(err.contains(bad), "{bad}: {err}");
        }
    }

    #[test]
    fn opclasses_follow_metric() {
        assert_eq!(
            PgPineconeBackend::opclass_for_metric(Metric::Angular),
            "vector_cosine_ops"
        );
        assert_eq!(
            PgPineconeBackend::opclass_for_metric(Metric::Euclidean),
            "vector_l2_ops"
        );
    }

    #[test]
    fn pgvector_is_created_before_pg_pinecone() {
        assert_eq!(
            PgPineconeBackend::extension_names(),
            &["vector", "pg_pinecone"]
        );
    }
}
