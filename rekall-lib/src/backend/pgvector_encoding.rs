// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

//! Shared operand-encoding model for the pgvector backends (HNSW and
//! IVFFlat). Both index methods support the same three encodings and the
//! same expression-index + optional rerank pattern, so the encoding logic
//! lives here once.

use serde::{Deserialize, Serialize};

use crate::config::Metric;

/// The operand encoding a pgvector index (HNSW or IVFFlat) is built over. All three are
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
    pub fn order_expr(self, dim: u32) -> Option<String> {
        match self {
            Self::Vector => None,
            Self::Halfvec => Some(format!("v::halfvec({dim})")),
            Self::Bit => Some(format!("binary_quantize(v)::bit({dim})")),
        }
    }

    /// Whether the encoding is lossy — i.e. a `rerank` query can recover
    /// recall by re-scoring the candidate pool on full-precision `v`.
    pub fn is_quantized(self) -> bool {
        self != Self::Vector
    }

    /// Full-precision distance operator on `v`, used by the rerank stage
    /// (which re-orders the candidate pool by the true metric).
    pub fn full_order_op(self, metric: Metric) -> &'static str {
        match metric {
            Metric::Angular => "<=>",
            Metric::Euclidean => "<->",
        }
    }

    /// Operator class for `CREATE INDEX ... USING hnsw (target opclass)`.
    /// Bit always uses hamming; the (rare) jaccard case isn't exposed.
    pub fn opclass(self, metric: Metric) -> &'static str {
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
    pub fn order_op(self, metric: Metric) -> &'static str {
        match (self, metric) {
            (Self::Vector | Self::Halfvec, Metric::Angular) => "<=>",
            (Self::Vector | Self::Halfvec, Metric::Euclidean) => "<->",
            (Self::Bit, _) => "<~>",
        }
    }

    /// SQL fragment that casts the bound `$1` (sent as a `vector`) to the
    /// operand type the index expects. Used in the prepared search
    /// statement for the quantized encodings.
    pub fn bind_expr(self, dim: u32) -> String {
        match self {
            Self::Vector => "$1".to_string(),
            Self::Halfvec => format!("($1::vector({dim}))::halfvec({dim})"),
            Self::Bit => format!("binary_quantize($1::vector({dim}))::bit({dim})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn bind_expr_shapes() {
        assert_eq!(
            Encoding::Bit.bind_expr(768),
            "binary_quantize($1::vector(768))::bit(768)"
        );
        assert_eq!(
            Encoding::Halfvec.bind_expr(768),
            "($1::vector(768))::halfvec(768)"
        );
        assert_eq!(Encoding::Vector.bind_expr(768), "$1");
    }
}
