//! pgvector HNSW backend.
//!
//! Supports three column types via the `column_type` param:
//!   - `vector`  (default) — full f32 vectors, indexed with `vector_*_ops`.
//!   - `halfvec`           — f16 half-precision, ~2x smaller index.
//!   - `bit`               — binary-quantized via `binary_quantize(v)`,
//!     ~32x smaller index, indexed with `bit_hamming_ops`.
//!
//! For non-vector variants the backend `ALTER TABLE`s a quantized column
//! into the shared rekall table, populates it from `v`, then builds the
//! HNSW index on the quantized column. The plain `v` column is never
//! modified, so other backends operating on the same table see the
//! schema they expect. The quantized column is dropped in `drop_index`
//! to keep sweeps idempotent.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::{Error, Result};

use super::postgres::PgConnection;
use super::IndexBackend;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ColumnType {
    #[default]
    Vector,
    Halfvec,
    Bit,
}

impl ColumnType {
    /// Column added to the rekall table for non-Vector variants.
    /// Vector indexes the existing `v` column directly.
    fn quantized_column(self) -> Option<&'static str> {
        match self {
            Self::Vector => None,
            Self::Halfvec => Some("v_half"),
            Self::Bit => Some("v_bit"),
        }
    }

    /// PG type used in `ALTER TABLE ADD COLUMN ... TYPE(dim)`.
    fn quantized_type(self) -> Option<&'static str> {
        match self {
            Self::Vector => None,
            Self::Halfvec => Some("halfvec"),
            Self::Bit => Some("bit"),
        }
    }

    /// SQL expression that converts `v` to the quantized column value.
    fn quantize_expr_from_v(self, dim: u32) -> Option<String> {
        match self {
            Self::Vector => None,
            Self::Halfvec => Some(format!("v::halfvec({dim})")),
            Self::Bit => Some(format!("binary_quantize(v)::bit({dim})")),
        }
    }

    /// Operator class for `CREATE INDEX ... USING hnsw (col opclass)`.
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

    /// Distance operator used in `ORDER BY col <op> $1`.
    fn order_op(self, metric: Metric) -> &'static str {
        match (self, metric) {
            (Self::Vector | Self::Halfvec, Metric::Angular) => "<=>",
            (Self::Vector | Self::Halfvec, Metric::Euclidean) => "<->",
            (Self::Bit, _) => "<~>",
        }
    }

    /// SQL fragment that casts the bound `$1` (sent as a `vector`) to
    /// the operand type the index expects. Used in the prepared search
    /// statement for non-vector variants.
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
    #[serde(default)]
    column_type: ColumnType,
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
}

impl PgvectorHnswBackend {
    pub async fn create(
        conn: &ConnectionConfig,
        table: &str,
        dim: u32,
        metric: Metric,
        idx: &IndexConfig,
        cancel_registry: Option<&crate::CancelRegistry>,
    ) -> Result<Self> {
        let params: HnswParams = idx.params.clone().try_into().map_err(|e| {
            Error::Config(format!(
                "Invalid pgvector-hnsw params for '{}': {e}",
                idx.name
            ))
        })?;

        let mut pg = PgConnection::new(conn, table, dim, metric);
        if let Some(cr) = cancel_registry {
            pg.set_cancel_registry(cr.clone());
        }
        let mut backend = Self {
            pg,
            name: idx.name.clone(),
            params,
        };
        // Trait-default connect uses query_column()/query_bind() to
        // build the search statement for whichever column_type this
        // backend was configured with (vector/halfvec/bit).
        backend.connect().await?;
        Ok(backend)
    }
}

#[async_trait]
impl IndexBackend for PgvectorHnswBackend {
    fn type_name(&self) -> &str {
        "pgvector-hnsw"
    }

    fn index_name(&self) -> &str {
        &self.name
    }

    fn pg(&self) -> &PgConnection {
        &self.pg
    }

    fn pg_mut(&mut self) -> &mut PgConnection {
        &mut self.pg
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["vector"]
    }

    fn order_op(&self) -> &'static str {
        self.params.column_type.order_op(self.pg.metric)
    }

    fn index_method(&self) -> &str {
        "hnsw"
    }

    fn opclass(&self) -> &str {
        self.params.column_type.opclass(self.pg.metric)
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

    fn derived_column(&self) -> Option<&str> {
        self.params.column_type.quantized_column()
    }

    fn derived_column_type(&self) -> Option<String> {
        self.params
            .column_type
            .quantized_type()
            .map(|ty| format!("{ty}({})", self.pg.dimension))
    }

    fn derive_from_v_expr(&self) -> Option<String> {
        self.params
            .column_type
            .quantize_expr_from_v(self.pg.dimension)
    }

    fn query_column(&self) -> &'static str {
        self.params.column_type.quantized_column().unwrap_or("v")
    }

    fn query_bind(&self) -> String {
        self.params.column_type.bind_expr(self.pg.dimension)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_default_column_type_is_vector() {
        let v: HnswParams = toml::from_str("").unwrap();
        assert_eq!(v.m, 16);
        assert_eq!(v.ef_construction, 200);
        assert_eq!(v.column_type, ColumnType::Vector);
    }

    #[test]
    fn params_parse_halfvec() {
        let v: HnswParams = toml::from_str("column_type = \"halfvec\"").unwrap();
        assert_eq!(v.column_type, ColumnType::Halfvec);
    }

    #[test]
    fn params_parse_bit() {
        let v: HnswParams = toml::from_str("column_type = \"bit\"").unwrap();
        assert_eq!(v.column_type, ColumnType::Bit);
    }

    #[test]
    fn opclass_matches_column_type() {
        assert_eq!(
            ColumnType::Vector.opclass(Metric::Angular),
            "vector_cosine_ops"
        );
        assert_eq!(
            ColumnType::Halfvec.opclass(Metric::Angular),
            "halfvec_cosine_ops"
        );
        assert_eq!(
            ColumnType::Halfvec.opclass(Metric::Euclidean),
            "halfvec_l2_ops"
        );
        assert_eq!(
            ColumnType::Bit.opclass(Metric::Angular),
            "bit_hamming_ops"
        );
        assert_eq!(
            ColumnType::Bit.opclass(Metric::Euclidean),
            "bit_hamming_ops"
        );
    }

    #[test]
    fn order_op_for_bit_is_hamming() {
        assert_eq!(ColumnType::Bit.order_op(Metric::Angular), "<~>");
        assert_eq!(ColumnType::Bit.order_op(Metric::Euclidean), "<~>");
    }

    #[test]
    fn quantize_expr_for_halfvec_includes_dim() {
        assert_eq!(
            ColumnType::Halfvec.quantize_expr_from_v(768).as_deref(),
            Some("v::halfvec(768)")
        );
    }

    #[test]
    fn quantize_expr_for_bit_uses_binary_quantize() {
        assert_eq!(
            ColumnType::Bit.quantize_expr_from_v(768).as_deref(),
            Some("binary_quantize(v)::bit(768)")
        );
    }

    #[test]
    fn bind_expr_for_bit_wraps_binary_quantize() {
        assert_eq!(
            ColumnType::Bit.bind_expr(768),
            "binary_quantize($1::vector(768))::bit(768)"
        );
    }
}
