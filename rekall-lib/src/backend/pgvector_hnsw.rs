//! pgvector HNSW backend.
//!
//! Supports three operand encodings via the `encoding` param (legacy alias
//! `column_type`):
//!   - `vector`  (default) — full f32 vectors, indexed with `vector_*_ops`.
//!   - `halfvec`           — f16 half-precision via `v::halfvec(N)`,
//!     ~2x smaller index.
//!   - `bit`               — binary-quantized via `binary_quantize(v)::bit(N)`,
//!     ~32x smaller index, ranked by Hamming (`bit_hamming_ops`).
//!
//! All three follow pgvector's documented recommendation and build
//! **expression indexes** directly over `v` (e.g.
//! `((binary_quantize(v)::bit(N)) bit_hamming_ops)`) — no materialized
//! column is ever added. `vector` indexes the bare `v` column. The shared
//! rekall table's schema is never modified, so other backends on the same
//! table see exactly what they expect.
//!
//! A quantized (`halfvec`/`bit`) index can be searched two ways (see
//! [`QueryMode`]): the raw single-stage query in the index's own distance,
//! or a two-stage `rerank` that over-fetches a candidate pool from the index
//! then re-orders it by full-precision distance on `v`. The pool size is the
//! swept `rerank_pool` param.

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
struct HnswParams {
    #[serde(default = "default_m")]
    m: u32,
    #[serde(default = "default_ef_construction")]
    ef_construction: u32,
    /// Operand encoding. `column_type` is accepted as a legacy alias.
    #[serde(default, alias = "column_type")]
    encoding: Encoding,
}

fn default_m() -> u32 {
    16
}
fn default_ef_construction() -> u32 {
    200
}

pub struct PgvectorHnswBackend {
    pg: PgConnection,
    name: String,
    params: HnswParams,
    /// Search shape this backend instance is wired for. Determines the
    /// prepared statement built in `connect()`.
    query_mode: QueryMode,
    /// Candidate-pool size for rerank queries (bound as `LIMIT $3`). Updated
    /// per sweep point via `set_search_params`'s reserved `rerank_pool` key.
    rerank_pool: AtomicI64,
}

impl PgvectorHnswBackend {
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
            "pgvector-hnsw",
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
impl IndexBackend for PgvectorHnswBackend {
    pg_backend_field_accessors!();

    fn type_name(&self) -> &str {
        "pgvector-hnsw"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["vector"]
    }

    fn order_op(&self) -> &'static str {
        self.params.encoding.order_op(self.pg.metric)
    }

    fn index_method(&self) -> &str {
        "hnsw"
    }

    fn opclass(&self) -> &str {
        self.params.encoding.opclass(self.pg.metric)
    }

    fn index_options(&self) -> Vec<(&'static str, String)> {
        vec![
            ("m", self.params.m.to_string()),
            ("ef_construction", self.params.ef_construction.to_string()),
        ]
    }

    fn build_params_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.params).unwrap_or_default()
    }

    /// Separate Pareto series by operand encoding. Full `vector` keeps the
    /// bare "pgvector HNSW" family label; the quantized encodings get their
    /// own frontier (`halfvec` / `bit`).
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
    /// other params are ordinary search GUCs.
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
    fn params_default_encoding_is_vector() {
        let v: HnswParams = toml::from_str("").unwrap();
        assert_eq!(v.m, 16);
        assert_eq!(v.ef_construction, 200);
        assert_eq!(v.encoding, Encoding::Vector);
    }

    #[test]
    fn params_parse_halfvec() {
        let v: HnswParams = toml::from_str("encoding = \"halfvec\"").unwrap();
        assert_eq!(v.encoding, Encoding::Halfvec);
    }

    #[test]
    fn params_parse_bit() {
        let v: HnswParams = toml::from_str("encoding = \"bit\"").unwrap();
        assert_eq!(v.encoding, Encoding::Bit);
    }

    #[test]
    fn column_type_is_accepted_as_legacy_alias() {
        let v: HnswParams = toml::from_str("column_type = \"bit\"").unwrap();
        assert_eq!(v.encoding, Encoding::Bit);
    }
}
