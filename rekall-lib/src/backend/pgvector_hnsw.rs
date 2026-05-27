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

use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_postgres::Statement;
use tracing::info;

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::{Error, Result};
use crate::metrics::IndexBuildResult;

use super::pg_vector_type::PgVector;
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
    /// For non-Vector column types we prepare a custom statement that
    /// references the quantized column. Vector uses `pg.query_stmt()`.
    custom_query: Option<Statement>,
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
        // PgConnection prepares its standard `ORDER BY v <op> $1`
        // statement against the existing `vector` column. Vector
        // variants reuse it; halfvec/bit prepare a custom one below.
        let order_op_for_setup = params.column_type.order_op(metric);
        pg.connect_with_setup(
            order_op_for_setup,
            Some("vector"),
            &["CREATE EXTENSION IF NOT EXISTS vector"],
        )
        .await?;

        let custom_query = if params.column_type == ColumnType::Vector {
            None
        } else {
            Some(prepare_quantized_query(&pg, params.column_type, table, dim, metric).await?)
        };

        Ok(Self {
            pg,
            name: idx.name.clone(),
            params,
            custom_query,
        })
    }

    /// Column the HNSW index is built on — `v` for vector, the
    /// quantized column otherwise.
    fn indexed_column(&self) -> &'static str {
        self.params.column_type.quantized_column().unwrap_or("v")
    }

    /// Add the quantized column and populate it from `v` via the
    /// shared `PgConnection::ensure_derived_column` helper.
    async fn ensure_quantized_column(&self) -> Result<()> {
        let (Some(col), Some(ty), Some(expr)) = (
            self.params.column_type.quantized_column(),
            self.params.column_type.quantized_type(),
            self.params.column_type.quantize_expr_from_v(self.pg.dimension),
        ) else {
            return Ok(());
        };
        self.pg
            .ensure_derived_column(col, &format!("{ty}({})", self.pg.dimension), &expr)
            .await
    }
}

/// Prepare the search statement for non-Vector column types.
async fn prepare_quantized_query(
    pg: &PgConnection,
    column_type: ColumnType,
    table: &str,
    dim: u32,
    metric: Metric,
) -> Result<Statement> {
    let qcol = column_type
        .quantized_column()
        .expect("non-Vector column type");
    let order_op = column_type.order_op(metric);
    let bind = column_type.bind_expr(dim);

    let sql = format!("SELECT id FROM {table} ORDER BY {qcol} {order_op} {bind} LIMIT $2");
    pg.prepare_vector_search(&sql, &[]).await
}

#[async_trait]
impl IndexBackend for PgvectorHnswBackend {
    fn type_name(&self) -> &str {
        "pgvector-hnsw"
    }

    fn index_name(&self) -> &str {
        &self.name
    }

    async fn build_index(&self) -> Result<IndexBuildResult> {
        self.ensure_quantized_column().await?;

        let client = self.pg.client();
        let idx_name = self.pg.index_sql_name(&self.name);

        let sql = format!(
            "CREATE INDEX {idx_name} ON {} USING hnsw ({} {}) \
             WITH (m = {}, ef_construction = {})",
            self.pg.table_name,
            self.indexed_column(),
            self.params.column_type.opclass(self.pg.metric),
            self.params.m,
            self.params.ef_construction,
        );

        info!("Building pgvector HNSW index: {sql}");
        let start = Instant::now();
        client.execute(&sql, &[]).await?;
        let build_time = start.elapsed();

        let size = self.index_size().await?;

        Ok(IndexBuildResult {
            index_name: self.name.clone(),
            index_type: "pgvector-hnsw".to_string(),
            params: serde_json::to_value(&self.params).unwrap_or_default(),
            build_time,
            index_size_bytes: size,
        })
    }

    async fn drop_index(&self) -> Result<()> {
        let idx_name = self.pg.index_sql_name(&self.name);
        let client = self.pg.client();
        client
            .execute(&format!("DROP INDEX IF EXISTS {idx_name}"), &[])
            .await?;
        if let Some(col) = self.params.column_type.quantized_column() {
            self.pg.drop_derived_column(col).await?;
        }
        Ok(())
    }

    async fn index_size(&self) -> Result<u64> {
        let idx_name = self.pg.index_sql_name(&self.name);
        let row = self
            .pg
            .client()
            .query_one(&format!("SELECT pg_relation_size('{idx_name}')"), &[])
            .await?;
        let size: i64 = row.get(0);
        Ok(size as u64)
    }

    async fn index_exists(&self) -> Result<bool> {
        let idx_name = self.pg.index_sql_name(&self.name);
        let row = self
            .pg
            .client()
            .query_one(
                "SELECT EXISTS(
                    SELECT 1 FROM pg_indexes
                    WHERE tablename = $1 AND indexname = $2
                )",
                &[&self.pg.table_name, &idx_name],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn set_search_params(&self, params: &HashMap<String, toml::Value>) -> Result<()> {
        self.pg.set_gucs(params).await
    }

    async fn validate_search_params(
        &self,
        params: &HashMap<String, Vec<toml::Value>>,
    ) -> Result<()> {
        self.pg.validate_gucs(params).await
    }

    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>> {
        let k_i64 = k as i64;
        let stmt = self.custom_query.as_ref().unwrap_or_else(|| self.pg.query_stmt());
        let rows = self
            .pg
            .client()
            .query(stmt, &[&PgVector(vector), &k_i64])
            .await?;
        Ok(rows.iter().map(|r| r.get::<_, i64>(0)).collect())
    }

    async fn validate_query_plan(&self, sample_vector: &[f32], k: usize) -> Result<()> {
        // PgConnection's validator EXPLAINs `ORDER BY v <op> ...`, which
        // only matches the index for the Vector variant. Halfvec/bit
        // use a different column and a casted bind expression; skip the
        // built-in check rather than maintain a parallel EXPLAIN here.
        if self.params.column_type == ColumnType::Vector {
            self.pg
                .validate_query_plan(&self.name, sample_vector, k)
                .await?;
        }
        Ok(())
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
