// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

//! ParadeDB (`pg_search`) vector search backend.
//!
//! ParadeDB indexes `v` in its own `paradedb` index (Tantivy segments, each
//! with a clustered vector index) and answers top-k through its custom scan.
//! That scan only takes over when the query has a ParadeDB search predicate,
//! so the search SQL matches every row with `v @@@ pdb.all()`. The index is
//! vector-only, and `id` comes from the heap for the k results.
//!
//! `pg_search` must be in `shared_preload_libraries`, and ParadeDB allows one
//! ParadeDB index per table, whether enabled or not.

use async_trait::async_trait;
use postgres_types::Type;
use serde::{Deserialize, Serialize};

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::{Error, Result};

use super::postgres::PgConnection;
use super::{pg_backend_field_accessors, IndexBackend};

/// Build-time reloptions. `None` leaves ParadeDB's default.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParadedbParams {
    /// Segments the build produces. ParadeDB's default is
    /// max(max_parallel_workers_per_gather, 4) in the build session.
    #[serde(default)]
    target_segment_count: Option<u32>,
    /// Target vectors per cluster. ParadeDB default: 400.
    #[serde(default)]
    target_leaf_size: Option<u32>,
    /// Fraction of vectors sampled to train the clusters; `0` lets ParadeDB
    /// choose from `maintenance_work_mem`. ParadeDB default: 0.32.
    #[serde(default)]
    training_sample_ratio: Option<f64>,
    /// Clusters each vector is assigned to (1 or 2). ParadeDB default: 2.
    #[serde(default)]
    replication: Option<u32>,
    /// One-bit centroid routing. ParadeDB default: true.
    #[serde(default)]
    vector_router_quantization: Option<bool>,
    /// Quantization layers. ParadeDB default: `[1, 1]`.
    #[serde(default)]
    quantization: Option<Quantization>,
}

/// `quantization` as written in TOML: `0` turns it off, an array gives the
/// bits of each layer in scoring order (e.g. `[1, 1]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum Quantization {
    Off(u8),
    Layers(Vec<u8>),
}

impl Quantization {
    /// The reloption's string value, quoted for the WITH clause.
    fn sql_value(&self) -> String {
        match self {
            Self::Off(_) => "'0'".to_string(),
            Self::Layers(bits) => {
                let bits: Vec<String> = bits.iter().map(u8::to_string).collect();
                format!("'[{}]'", bits.join(","))
            }
        }
    }
}

fn parse_params(value: &toml::Value) -> std::result::Result<ParadedbParams, String> {
    let params: ParadedbParams = value.clone().try_into().map_err(|e| format!("{e}"))?;
    if let Some(Quantization::Off(n)) = params.quantization {
        if n != 0 {
            return Err(format!(
                "quantization = {n}: use 0 to turn it off, or an array of \
                 layer widths such as [1, 1]"
            ));
        }
    }
    Ok(params)
}

pub struct ParadedbBackend {
    pg: PgConnection,
    name: String,
    params: ParadedbParams,
}

impl ParadedbBackend {
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
            "paradedb",
            |pg, name, params| Self { pg, name, params },
        )
        .await
    }

    fn order_op_for_metric(metric: Metric) -> &'static str {
        match metric {
            Metric::Angular => "<=>",
            Metric::Euclidean => "<->",
        }
    }

    fn opclass_for_metric(metric: Metric) -> &'static str {
        match metric {
            Metric::Angular => "vector_cosine_ops",
            Metric::Euclidean => "vector_l2_ops",
        }
    }
}

#[async_trait]
impl IndexBackend for ParadedbBackend {
    pg_backend_field_accessors!();

    fn type_name(&self) -> &str {
        "paradedb"
    }

    fn type_names() -> &'static [&'static str] {
        &["paradedb"]
    }

    fn validate_params(idx: &IndexConfig) -> Result<()> {
        parse_params(&idx.params)
            .map(|_| ())
            .map_err(|e| super::invalid_params(idx, e))
    }

    fn single_index_per_table() -> bool {
        true
    }

    /// pgvector first: ParadeDB indexes pgvector's `vector` type.
    fn extensions(&self) -> &'static [&'static str] {
        &["vector", "pg_search"]
    }

    fn order_op(&self) -> &'static str {
        Self::order_op_for_metric(self.pg.metric)
    }

    fn index_method(&self) -> &str {
        "paradedb"
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

    fn custom_search_sql(&self) -> Option<(String, Vec<Type>)> {
        Some((search_sql(&self.pg.table_name, self.order_op()), Vec::new()))
    }

    /// ParadeDB refuses a second ParadeDB index on a table. Name the one in
    /// the way and how to remove it, rather than fail inside CREATE INDEX.
    async fn check_can_build(&self) -> Result<()> {
        let own = self.pg.index_sql_name(&self.name);
        let row = self
            .pg
            .client()
            .query_opt(
                "SELECT c.relname::text FROM pg_index i \
                 JOIN pg_class c ON c.oid = i.indexrelid \
                 JOIN pg_am a ON a.oid = c.relam \
                 WHERE i.indrelid = $1::text::regclass \
                   AND a.amname IN ('paradedb', 'bm25') \
                   AND c.relname <> $2 \
                 LIMIT 1",
                &[&self.pg.table_name, &own],
            )
            .await?;
        match row {
            None => Ok(()),
            Some(row) => {
                let existing: String = row.get(0);
                Err(Error::Config(format!(
                    "table '{}' already has ParadeDB index '{existing}', and \
                     ParadeDB allows only one per table, so '{}' cannot be \
                     built. Drop it first: DROP INDEX {existing};",
                    self.pg.table_name, self.name,
                )))
            }
        }
    }

    async fn index_layout(&self) -> Result<Option<serde_json::Value>> {
        let idx = self.pg.index_sql_name(&self.name);
        let row = self
            .pg
            .client()
            .query_one(
                "SELECT json_build_object( \
                     'segments', count(*), \
                     'ivf_segments', count(*) FILTER (WHERE vector_format = 'ivf'), \
                     'vectors', sum(vector_num_vectors)::bigint, \
                     'centroids', sum(vector_num_centroids)::bigint, \
                     'quantized_rows', sum(vector_total_rows)::bigint, \
                     'quantization', string_agg(DISTINCT layers::text, ' ')) \
                 FROM paradedb.vector_info($1::text::regclass, 'v')",
                &[&idx],
            )
            .await?;
        Ok(Some(row.get(0)))
    }

    /// The default check only covers the standard query shape. Here both
    /// the custom and the generic plan of the prepared search must run
    /// ParadeDB's top-k scan on this arm's index: without it ParadeDB still
    /// answers, but by sorting every row.
    async fn validate_query_plan(&self, sample_vector: &[f32], k: usize) -> Result<()> {
        let idx = self.pg.index_sql_name(&self.name);
        let sql = search_sql(&self.pg.table_name, self.order_op());
        for (label, plan) in self.pg.explain_search_plans(&sql, sample_vector, k).await? {
            check_topk_plan(&plan, &idx).map_err(|problem| {
                Error::Config(format!(
                    "The {label} query plan {problem}.\n  SQL: {sql}\nPlan:\n{plan}"
                ))
            })?;
            if plan.contains("Gather") {
                tracing::warn!(
                    "The {label} plan for '{}' runs parallel workers; \
                     per-query CPU differs from single-process backends.\n{plan}",
                    self.name
                );
            }
        }
        Ok(())
    }
}

/// Top-k search over all rows. `$1` is the query vector, `$2` the LIMIT.
fn search_sql(table: &str, order_op: &str) -> String {
    format!("SELECT id FROM {table} WHERE v @@@ pdb.all() ORDER BY v {order_op} $1 LIMIT $2")
}

/// Checks that an EXPLAIN plan runs ParadeDB's top-k scan on `index`. The
/// error completes the sentence "The query plan ...".
fn check_topk_plan(plan: &str, index: &str) -> std::result::Result<(), String> {
    if !plan.contains("Custom Scan (ParadeDB Base Scan)") {
        return Err("does not use the ParadeDB custom scan".to_string());
    }
    let index_line = format!("Index: {index}");
    if !plan.lines().any(|line| line.trim() == index_line) {
        return Err(format!("does not scan ParadeDB index '{index}'"));
    }
    if !plan.contains("Exec Method: TopKScanExecState") {
        return Err("does not use ParadeDB's top-k scan (TopKScanExecState), \
                    so it would sort every row"
            .to_string());
    }
    Ok(())
}

/// The `CREATE INDEX ... WITH (...)` list. Kept free of the backend so tests
/// need no connection.
fn options_from_params(params: &ParadedbParams) -> Vec<(&'static str, String)> {
    let mut opts = Vec::new();
    if let Some(n) = params.target_segment_count {
        opts.push(("target_segment_count", n.to_string()));
    }
    if let Some(n) = params.target_leaf_size {
        opts.push(("target_leaf_size", n.to_string()));
    }
    if let Some(r) = params.training_sample_ratio {
        opts.push(("training_sample_ratio", r.to_string()));
    }
    if let Some(n) = params.replication {
        opts.push(("replication", n.to_string()));
    }
    // Forwarded both ways so the build does not depend on ParadeDB's
    // current default.
    if let Some(b) = params.vector_router_quantization {
        opts.push(("vector_router_quantization", b.to_string()));
    }
    if let Some(ref q) = params.quantization {
        opts.push(("quantization", q.sql_value()));
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(value: toml::Value) -> ParadedbParams {
        parse_params(&value).unwrap()
    }

    /// EXPLAIN EXECUTE output from pg_search 0.27.0 for the search SQL,
    /// as a custom plan.
    const TOPK_CUSTOM_PLAN: &str = "\
Limit  (cost=10.00..10.06 rows=5 width=16)
  ->  Custom Scan (ParadeDB Base Scan) on t4  (cost=10.00..10.06 rows=5 width=16)
        Table: t4
        Index: idx_rekall_t4_paradedb
        Segment Count: 1
        Exec Method: TopKScanExecState
        Scores: false
           TopK Order By: v <-> vector asc
           TopK Limit: 5
        Full Index Scan: true
        Tantivy Query: {\"with_index\":{\"query\":{\"all\":{\"field\":\"v\"}}}}";

    /// The same statement as a generic plan: the LIMIT is a parameter.
    const TOPK_GENERIC_PLAN: &str = "\
Limit  (cost=10.00..11.25 rows=100 width=16)
  ->  Custom Scan (ParadeDB Base Scan) on t4  (cost=10.00..22.50 rows=1000 width=16)
        Table: t4
        Index: idx_rekall_t4_paradedb
        Segment Count: 1
        Exec Method: TopKScanExecState
        Scores: false
           TopK Order By: v <-> vector asc
        Full Index Scan: true
        Tantivy Query: {\"with_index\":{\"query\":{\"all\":{\"field\":\"v\"}}}}";

    /// An ORDER BY operand ParadeDB cannot push down (a sub-select): it
    /// still scans the ParadeDB index, then sorts every row.
    const SORTED_PLAN: &str = "\
Limit  (cost=50.15..50.16 rows=5 width=16)
  InitPlan 1
    ->  Seq Scan on t4 t4_1  (cost=0.00..40.00 rows=10 width=32)
          Filter: (id = '1'::bigint)
  ->  Sort  (cost=10.15..10.16 rows=5 width=16)
        Sort Key: ((t4.v <-> (InitPlan 1).col1))
        ->  Custom Scan (ParadeDB Base Scan) on t4  (cost=10.00..10.09 rows=5 width=16)
              Table: t4
              Index: idx_rekall_t4_paradedb
              Segment Count: 1
              Exec Method: NormalScanExecState
              Scores: false
              Full Index Scan: true
              Tantivy Query: {\"with_index\":{\"query\":{\"all\":{\"field\":\"v\"}}}}";

    const SEQ_SCAN_PLAN: &str = "\
Limit
  ->  Sort
        Sort Key: ((v <-> '[0,0,0]'::vector))
        ->  Seq Scan on t3";

    #[test]
    fn defaults_emit_no_options() {
        let empty = toml::Value::Table(Default::default());
        assert!(options_from_params(&params(empty)).is_empty());
    }

    #[test]
    fn every_param_maps_to_its_reloption() {
        let p = params(
            toml::toml! {
                target_segment_count = 8
                target_leaf_size = 300
                training_sample_ratio = 0.5
                replication = 1
                vector_router_quantization = true
                quantization = [2, 1]
            }
            .into(),
        );
        assert_eq!(
            options_from_params(&p),
            vec![
                ("target_segment_count", "8".to_string()),
                ("target_leaf_size", "300".to_string()),
                ("training_sample_ratio", "0.5".to_string()),
                ("replication", "1".to_string()),
                ("vector_router_quantization", "true".to_string()),
                ("quantization", "'[2,1]'".to_string()),
            ]
        );
    }

    #[test]
    fn router_quantization_false_is_emitted() {
        let p = params(toml::toml! { vector_router_quantization = false }.into());
        assert_eq!(
            options_from_params(&p),
            vec![("vector_router_quantization", "false".to_string())]
        );
    }

    #[test]
    fn quantization_zero_turns_it_off() {
        let p = params(toml::toml! { quantization = 0 }.into());
        assert_eq!(
            options_from_params(&p),
            vec![("quantization", "'0'".to_string())]
        );
    }

    #[test]
    fn quantization_other_integers_are_rejected() {
        let err = parse_params(&toml::toml! { quantization = 2 }.into()).unwrap_err();
        assert!(err.contains("quantization = 2"), "{err}");
    }

    #[test]
    fn unknown_param_is_rejected_by_name() {
        let err = parse_params(&toml::toml! { target_segments = 8 }.into()).unwrap_err();
        assert!(err.contains("target_segments"), "{err}");
    }

    #[test]
    fn search_sql_matches_all_rows_through_paradedb() {
        assert_eq!(
            search_sql("cohere_1m", "<=>"),
            "SELECT id FROM cohere_1m WHERE v @@@ pdb.all() ORDER BY v <=> $1 LIMIT $2"
        );
    }

    #[test]
    fn opclass_and_operator_follow_metric() {
        assert_eq!(
            ParadedbBackend::opclass_for_metric(Metric::Angular),
            "vector_cosine_ops"
        );
        assert_eq!(
            ParadedbBackend::opclass_for_metric(Metric::Euclidean),
            "vector_l2_ops"
        );
        assert_eq!(ParadedbBackend::order_op_for_metric(Metric::Angular), "<=>");
        assert_eq!(
            ParadedbBackend::order_op_for_metric(Metric::Euclidean),
            "<->"
        );
    }

    #[test]
    fn topk_plans_pass() {
        assert_eq!(
            check_topk_plan(TOPK_CUSTOM_PLAN, "idx_rekall_t4_paradedb"),
            Ok(())
        );
        assert_eq!(
            check_topk_plan(TOPK_GENERIC_PLAN, "idx_rekall_t4_paradedb"),
            Ok(())
        );
    }

    #[test]
    fn sorted_plan_is_rejected() {
        let err = check_topk_plan(SORTED_PLAN, "idx_rekall_t4_paradedb").unwrap_err();
        assert!(err.contains("TopKScanExecState"), "{err}");
    }

    #[test]
    fn plan_without_paradedb_scan_is_rejected() {
        let err = check_topk_plan(SEQ_SCAN_PLAN, "idx_rekall_t4_paradedb").unwrap_err();
        assert!(err.contains("ParadeDB custom scan"), "{err}");
    }

    #[test]
    fn plan_on_another_index_is_rejected() {
        // A name that only shares a prefix must not count as a match.
        let err = check_topk_plan(TOPK_CUSTOM_PLAN, "idx_rekall_t4_para").unwrap_err();
        assert!(err.contains("idx_rekall_t4_para'"), "{err}");
    }
}
