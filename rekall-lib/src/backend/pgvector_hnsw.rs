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
use super::postgres::PgConnection;
use super::{pg_backend_field_accessors, IndexBackend};

/// The operand encoding the HNSW index is built over. All three are
/// expression indexes (or, for `Vector`, the bare `v` column) — none
/// materializes a column on the shared rekall table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// Full f32 vectors — index `v` directly with `vector_*_ops`.
    #[default]
    Vector,
    /// f16 half-precision via `v::halfvec(N)` — ~2x smaller index.
    Halfvec,
    /// 1-bit binary quantization via `binary_quantize(v)::bit(N)` — ~32x
    /// smaller, ranked by Hamming distance (`bit_hamming_ops`).
    Bit,
}

impl Encoding {
    /// The encoded operand as it appears in a query `ORDER BY` (no outer
    /// parens), e.g. `binary_quantize(v)::bit(N)`. `None` for `Vector`,
    /// which is queried by column name.
    fn order_expr(self, dim: u32) -> Option<String> {
        match self {
            Self::Vector => None,
            Self::Halfvec => Some(format!("v::halfvec({dim})")),
            Self::Bit => Some(format!("binary_quantize(v)::bit({dim})")),
        }
    }

    /// Whether the encoding is lossy — i.e. a `rerank` query can recover
    /// recall by re-scoring the candidate pool on full-precision `v`.
    fn is_quantized(self) -> bool {
        self != Self::Vector
    }

    /// Full-precision distance operator on `v`, used by the rerank stage
    /// (which re-orders the candidate pool by the true metric).
    fn full_order_op(self, metric: Metric) -> &'static str {
        match metric {
            Metric::Angular => "<=>",
            Metric::Euclidean => "<->",
        }
    }

    /// Operator class for `CREATE INDEX ... USING hnsw (target opclass)`.
    /// Bit always uses hamming; the (rare) jaccard case isn't exposed.
    fn opclass(self, metric: Metric) -> &'static str {
        match (self, metric) {
            (Self::Vector, Metric::Angular) => "vector_cosine_ops",
            (Self::Vector, Metric::Euclidean) => "vector_l2_ops",
            (Self::Halfvec, Metric::Angular) => "halfvec_cosine_ops",
            (Self::Halfvec, Metric::Euclidean) => "halfvec_l2_ops",
            (Self::Bit, _) => "bit_hamming_ops",
        }
    }

    /// The index's own ranking operator, used in
    /// `ORDER BY <order_expr> <op> <bind>` (and `v <op> $1` for `Vector`).
    fn order_op(self, metric: Metric) -> &'static str {
        match (self, metric) {
            (Self::Vector | Self::Halfvec, Metric::Angular) => "<=>",
            (Self::Vector | Self::Halfvec, Metric::Euclidean) => "<->",
            (Self::Bit, _) => "<~>",
        }
    }

    /// SQL fragment that casts the bound `$1` (sent as a `vector`) to the
    /// operand type the index expects. Used in the prepared search
    /// statement for the quantized encodings.
    fn bind_expr(self, dim: u32) -> String {
        match self {
            Self::Vector => "$1".to_string(),
            Self::Halfvec => format!("($1::vector({dim}))::halfvec({dim})"),
            Self::Bit => format!("binary_quantize($1::vector({dim}))::bit({dim})"),
        }
    }
}

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

    #[test]
    fn opclass_matches_encoding() {
        assert_eq!(
            Encoding::Vector.opclass(Metric::Angular),
            "vector_cosine_ops"
        );
        assert_eq!(
            Encoding::Halfvec.opclass(Metric::Angular),
            "halfvec_cosine_ops"
        );
        assert_eq!(
            Encoding::Halfvec.opclass(Metric::Euclidean),
            "halfvec_l2_ops"
        );
        assert_eq!(Encoding::Bit.opclass(Metric::Angular), "bit_hamming_ops");
        assert_eq!(Encoding::Bit.opclass(Metric::Euclidean), "bit_hamming_ops");
    }

    #[test]
    fn order_op_for_bit_is_hamming() {
        assert_eq!(Encoding::Bit.order_op(Metric::Angular), "<~>");
        assert_eq!(Encoding::Bit.order_op(Metric::Euclidean), "<~>");
    }

    #[test]
    fn index_expr_parenthesizes_quantized_encodings() {
        // Quantized encodings are expression indexes — the shared
        // `expr_index::index_target` parenthesizes them so pgvector treats
        // them as such; `vector` indexes the bare column.
        let target = |e: Option<String>| e.map(|s| crate::backend::expr_index::index_target(&s));
        assert_eq!(
            target(Encoding::Bit.order_expr(768)).as_deref(),
            Some("(binary_quantize(v)::bit(768))")
        );
        assert_eq!(
            target(Encoding::Halfvec.order_expr(768)).as_deref(),
            Some("(v::halfvec(768))")
        );
        assert!(Encoding::Vector.order_expr(768).is_none());
    }

    #[test]
    fn order_expr_has_no_outer_parens() {
        assert_eq!(
            Encoding::Bit.order_expr(768).as_deref(),
            Some("binary_quantize(v)::bit(768)")
        );
        assert_eq!(
            Encoding::Halfvec.order_expr(768).as_deref(),
            Some("v::halfvec(768)")
        );
        assert!(Encoding::Vector.order_expr(768).is_none());
    }

    #[test]
    fn full_order_op_uses_true_metric() {
        assert_eq!(Encoding::Bit.full_order_op(Metric::Angular), "<=>");
        assert_eq!(Encoding::Bit.full_order_op(Metric::Euclidean), "<->");
    }

    #[test]
    fn is_quantized_only_for_halfvec_and_bit() {
        assert!(!Encoding::Vector.is_quantized());
        assert!(Encoding::Halfvec.is_quantized());
        assert!(Encoding::Bit.is_quantized());
    }

    #[test]
    fn bind_expr_for_bit_wraps_binary_quantize() {
        assert_eq!(
            Encoding::Bit.bind_expr(768),
            "binary_quantize($1::vector(768))::bit(768)"
        );
    }

    #[test]
    fn bind_expr_for_halfvec_casts_through_vector() {
        // The bound parameter is sent as `vector`; cast it to halfvec
        // on the right of the operator so the index expression matches.
        assert_eq!(
            Encoding::Halfvec.bind_expr(768),
            "($1::vector(768))::halfvec(768)"
        );
    }

    #[test]
    fn bind_expr_for_vector_is_plain_placeholder() {
        assert_eq!(Encoding::Vector.bind_expr(768), "$1");
    }
}
