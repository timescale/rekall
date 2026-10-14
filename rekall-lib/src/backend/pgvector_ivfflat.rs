// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

//! pgvector IVFFlat backend.
//!
//! Shares the operand-encoding model with the HNSW backend (see
//! [`super::pgvector_encoding::Encoding`]): full `vector`, `halfvec`, or
//! binary-quantized `bit`, each as an expression index over `v` (or the
//! bare column for `vector`) — the shared rekall table is never modified.
//!
//! IVFFlat's single build parameter is `lists` (the k-means partition
//! count; pgvector recommends `sqrt(rows)` beyond 1M rows). Search depth
//! is the `ivfflat.probes` GUC, swept like `hnsw.ef_search`. The build
//! trains k-means at `CREATE INDEX` time, so rekall's usual
//! load-then-index ordering is required for a meaningful index (an
//! empty-table build produces a degenerate partitioning).
//!
//! Quantized encodings support the same two-stage `rerank` query mode as
//! HNSW: over-fetch a candidate pool in the index's own distance, then
//! re-order by full-precision distance on `v` (pool size = the swept
//! `rerank_pool` param). Note that raw-mode recall for any encoding is
//! bounded by the probed lists' contents; unlike HNSW there is no
//! ef-style within-probe depth knob.

use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use postgres_types::Type;
use serde::{Deserialize, Serialize};

use crate::config::{ConnectionConfig, IndexConfig, Metric, QueryMode, RERANK_POOL_KEY};
use crate::error::{Error, Result};

use super::pg_vector_type::PgVector;
use super::pgvector_encoding::Encoding;
use super::postgres::PgConnection;
use super::{pg_backend_field_accessors, IndexBackend};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IvfflatParams {
    /// k-means partition count. No default: unlike HNSW's graph
    /// parameters, a sensible value is a function of the row count
    /// (pgvector: `rows / 1000` up to 1M rows, `sqrt(rows)` beyond), so
    /// every config must choose it for its dataset scale.
    lists: u32,
    /// Operand encoding. `column_type` is accepted as a legacy alias.
    #[serde(default, alias = "column_type")]
    encoding: Encoding,
}

pub(crate) fn parse_params(value: &toml::Value) -> std::result::Result<(), toml::de::Error> {
    let _: IvfflatParams = value.clone().try_into()?;
    Ok(())
}

pub struct PgvectorIvfflatBackend {
    pg: PgConnection,
    name: String,
    params: IvfflatParams,
    /// Search shape this backend instance is wired for. Determines the
    /// prepared statement built in `connect()`.
    query_mode: QueryMode,
    /// Candidate-pool size for rerank queries (bound as `LIMIT $3`). Updated
    /// per sweep point via `set_search_params`'s reserved `rerank_pool` key.
    rerank_pool: AtomicI64,
}

impl PgvectorIvfflatBackend {
    pub async fn create(
        conn: &ConnectionConfig,
        table: &str,
        dim: u32,
        metric: Metric,
        idx: &IndexConfig,
        cancel_registry: Option<&crate::CancelRegistry>,
        query_mode: QueryMode,
    ) -> Result<Self> {
        super::create_pg_backend(
            conn,
            table,
            dim,
            metric,
            idx,
            cancel_registry,
            "pgvector-ivfflat",
            move |pg, name, params| Self {
                pg,
                name,
                params,
                query_mode,
                rerank_pool: AtomicI64::new(0),
            },
        )
        .await
    }

    /// Whether this instance runs the two-stage rerank query (the quantized
    /// `halfvec`/`bit` encodings support it; `vector` is already exact).
    fn is_rerank(&self) -> bool {
        self.query_mode == QueryMode::Rerank && self.params.encoding.is_quantized()
    }
}

#[async_trait]
impl IndexBackend for PgvectorIvfflatBackend {
    pg_backend_field_accessors!();

    fn type_name(&self) -> &str {
        "pgvector-ivfflat"
    }

    fn type_names() -> &'static [&'static str] {
        &["pgvector-ivfflat"]
    }

    fn validate_params(idx: &IndexConfig) -> Result<()> {
        parse_params(&idx.params).map_err(|e| super::invalid_params(idx, e))
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["vector"]
    }

    fn order_op(&self) -> &'static str {
        self.params.encoding.order_op(self.pg.metric)
    }

    fn index_method(&self) -> &str {
        "ivfflat"
    }

    fn opclass(&self) -> &str {
        self.params.encoding.opclass(self.pg.metric)
    }

    fn index_options(&self) -> Vec<(&'static str, String)> {
        vec![("lists", self.params.lists.to_string())]
    }

    fn build_params_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.params).unwrap_or_default()
    }

    /// Separate Pareto series by operand encoding, mirroring HNSW.
    fn data_type_label(&self) -> Option<String> {
        match self.params.encoding {
            Encoding::Vector => None,
            Encoding::Halfvec => Some("halfvec".to_string()),
            Encoding::Bit => Some("bit".to_string()),
        }
    }

    /// `vector` indexes the bare `v` column; `halfvec`/`bit` index the
    /// parenthesized cast/quantize expression (an expression index, no
    /// materialized column). `derived_column` stays `None` for all three.
    fn index_target(&self) -> String {
        self.params
            .encoding
            .order_expr(self.pg.dimension)
            .map(|e| crate::backend::expr_index::index_target(&e))
            .unwrap_or_else(|| "v".to_string())
    }

    /// Quantized encodings (`halfvec`/`bit`) order by an expression — and
    /// the `rerank` variant wraps a candidate-pool subquery — so build
    /// their SQL explicitly. `vector` falls back to the default template.
    fn custom_search_sql(&self) -> Option<(String, Vec<Type>)> {
        let dim = self.pg.dimension;
        let order_expr = self.params.encoding.order_expr(dim)?;
        let bind = self.params.encoding.bind_expr(dim);
        let op = self.params.encoding.order_op(self.pg.metric);
        let table = &self.pg.table_name;

        if self.is_rerank() {
            // Over-fetch `$3` candidates in the index's own distance, then
            // re-rank that pool by full-precision distance on `v` (pgvector's
            // recommended two-stage quantized query).
            let full = self.params.encoding.full_order_op(self.pg.metric);
            Some((
                crate::backend::expr_index::rerank_sql(table, &order_expr, op, &bind, full),
                vec![Type::INT8],
            ))
        } else {
            Some((
                crate::backend::expr_index::search_sql(table, &order_expr, op, &bind),
                Vec::new(),
            ))
        }
    }

    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>> {
        let k_i64 = k as i64;
        let rows = if self.is_rerank() {
            let pool = self.rerank_pool.load(Ordering::Relaxed).max(k_i64);
            self.pg
                .client()
                .query(self.pg.query_stmt(), &[&PgVector(vector), &k_i64, &pool])
                .await?
        } else {
            self.pg
                .client()
                .query(self.pg.query_stmt(), &[&PgVector(vector), &k_i64])
                .await?
        };
        Ok(rows.iter().map(|r| r.get::<_, i64>(0)).collect())
    }

    /// Intercept the reserved `rerank_pool` key (the rerank candidate-pool
    /// `LIMIT`, bound at query time) so it isn't issued as a `SET`. All
    /// other params are ordinary search GUCs (`ivfflat.probes`).
    async fn set_search_params(
        &self,
        params: &std::collections::HashMap<String, toml::Value>,
    ) -> Result<()> {
        let mut gucs = params.clone();
        if let Some(value) = gucs.remove(RERANK_POOL_KEY) {
            let pool = value.as_integer().ok_or_else(|| {
                Error::Config(format!(
                    "'{RERANK_POOL_KEY}' must be an integer, got {value}"
                ))
            })?;
            self.rerank_pool.store(pool, Ordering::Relaxed);
        }
        self.pg.set_gucs(&gucs).await
    }

    async fn validate_search_params(
        &self,
        params: &std::collections::HashMap<String, Vec<toml::Value>>,
    ) -> Result<()> {
        let mut gucs = params.clone();
        if let Some(values) = gucs.remove(RERANK_POOL_KEY) {
            if values.is_empty() {
                return Err(Error::Config(format!("'{RERANK_POOL_KEY}' has no values")));
            }
            for v in &values {
                if v.as_integer().is_none() {
                    return Err(Error::Config(format!(
                        "'{RERANK_POOL_KEY}' values must be integers, got {v}"
                    )));
                }
            }
        }
        self.pg.validate_gucs(&gucs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_require_lists() {
        assert!(toml::from_str::<IvfflatParams>("").is_err());
    }

    #[test]
    fn params_default_encoding_is_vector() {
        let v: IvfflatParams = toml::from_str("lists = 3162").unwrap();
        assert_eq!(v.lists, 3162);
        assert_eq!(v.encoding, Encoding::Vector);
    }

    #[test]
    fn params_parse_encodings() {
        let v: IvfflatParams = toml::from_str("lists = 100\nencoding = \"halfvec\"").unwrap();
        assert_eq!(v.encoding, Encoding::Halfvec);
        let v: IvfflatParams = toml::from_str("lists = 100\nencoding = \"bit\"").unwrap();
        assert_eq!(v.encoding, Encoding::Bit);
    }

    #[test]
    fn column_type_is_accepted_as_legacy_alias() {
        let v: IvfflatParams = toml::from_str("lists = 100\ncolumn_type = \"bit\"").unwrap();
        assert_eq!(v.encoding, Encoding::Bit);
    }
}
