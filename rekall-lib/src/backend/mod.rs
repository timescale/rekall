pub mod meerkat;
pub mod pg_vector_type;
pub mod pgvector_hnsw;
pub mod postgres;
pub mod vchordrq;

use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use tokio_postgres::Statement;
use tracing::info;

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::{Error, Result};
use crate::metrics::IndexBuildResult;
use crate::progress::ProgressTx;

use pg_vector_type::PgVector;
use postgres::PgConnection;

/// Trait every PostgreSQL-backed vector index implements.
///
/// Backends are minimal: they declare identity (`type_name`,
/// `index_name`, `pg`), index shape (`index_method`, `opclass`,
/// optional `index_options`), and — when applicable — derived-column
/// info and a custom search statement. All boilerplate (CREATE INDEX
/// assembly + DROP + size/exists probes + GUC set/validate +
/// SELECT id FROM t ORDER BY v <op> $1 LIMIT $2 search statement +
/// EXPLAIN-based plan validation) is provided by default impls.
///
/// Hooks for derived-column workflows (halfvec, bit, rabitq{4,8}, ...):
///   - `derived_column()` + `derived_column_type()` +
///     `derive_from_v_expr()`: drive the default build_index to call
///     `PgConnection::ensure_derived_column` before CREATE INDEX, and
///     the default drop_index to drop the column afterwards.
///   - `custom_search_stmt()`: replaces the standard search statement
///     for both `query` and `validate_query_plan` (the latter is
///     skipped because the custom shape doesn't match the default
///     EXPLAIN check).
#[async_trait]
pub trait IndexBackend: Send + Sync {
    fn type_name(&self) -> &str;

    fn index_name(&self) -> &str;

    fn pg(&self) -> &PgConnection;

    /// Access method name used in `CREATE INDEX ... USING <method>`.
    /// e.g. "hnsw", "mktann", "vchordrq".
    fn index_method(&self) -> &str;

    /// Operator class for `CREATE INDEX (col <opclass>)`. Depends on
    /// the column type and the dataset metric.
    fn opclass(&self) -> &str;

    /// Key-value pairs for the `CREATE INDEX ... WITH (...)` clause.
    /// Values are pre-formatted SQL (so `("m", "16")` becomes
    /// `m = 16`, and `("options", "$$<toml>$$")` becomes
    /// `options = $$<toml>$$`). Default: empty (no WITH clause).
    fn index_options(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }

    /// Params summary embedded in the `IndexBuildResult`. Default: `{}`.
    fn build_params_json(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    /// Derived column to drop alongside the index, if any. Default: none.
    fn derived_column(&self) -> Option<&str> {
        None
    }

    /// PG type declaration for the derived column, e.g. `"halfvec(768)"`.
    /// Must be `Some` when `derived_column` is `Some`.
    fn derived_column_type(&self) -> Option<String> {
        None
    }

    /// SQL expression that populates the derived column from `v`,
    /// e.g. `"v::halfvec(768)"` or `"quantize_to_rabitq8(v)"`. Must be
    /// `Some` when `derived_column` is `Some`.
    fn derive_from_v_expr(&self) -> Option<String> {
        None
    }

    /// Custom prepared statement for the search query, if any. When
    /// `Some`, both `query` and `validate_query_plan` switch to the
    /// custom path; the default plan validator is skipped because it
    /// EXPLAINs the standard shape.
    fn custom_search_stmt(&self) -> Option<&Statement> {
        None
    }

    /// Build the index. Default impl assembles
    /// `CREATE INDEX ON t USING <method> (col <opclass>) WITH (...)`
    /// from `index_method`/`opclass`/`index_options`, ensuring the
    /// derived column exists first when applicable. Backends usually
    /// don't override this; the rabitq4-rerank case in vchordrq is the
    /// kind of exception where override is fine.
    async fn build_index(&self) -> Result<IndexBuildResult> {
        if let (Some(col), Some(ty), Some(expr)) = (
            self.derived_column(),
            self.derived_column_type(),
            self.derive_from_v_expr(),
        ) {
            self.pg().ensure_derived_column(col, &ty, &expr).await?;
        }

        let idx_name = self.pg().index_sql_name(self.index_name());
        let column = self.derived_column().unwrap_or("v");
        let opts = self.index_options();
        let with_clause = if opts.is_empty() {
            String::new()
        } else {
            let body = opts
                .iter()
                .map(|(k, v)| format!("{k} = {v}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" WITH ({body})")
        };
        let sql = format!(
            "CREATE INDEX {idx_name} ON {} USING {} ({column} {}){with_clause}",
            self.pg().table_name,
            self.index_method(),
            self.opclass(),
        );

        info!("Building {} index: {sql}", self.type_name());
        let start = Instant::now();
        self.pg().client().execute(&sql, &[]).await?;
        let build_time = start.elapsed();
        let size = self.index_size().await?;

        Ok(IndexBuildResult {
            index_name: self.index_name().to_string(),
            index_type: self.type_name().to_string(),
            params: self.build_params_json(),
            build_time,
            index_size_bytes: size,
        })
    }

    async fn drop_index(&self) -> Result<()> {
        let idx_name = self.pg().index_sql_name(self.index_name());
        self.pg()
            .client()
            .execute(&format!("DROP INDEX IF EXISTS {idx_name}"), &[])
            .await?;
        if let Some(col) = self.derived_column() {
            self.pg().drop_derived_column(col).await?;
        }
        Ok(())
    }

    async fn index_size(&self) -> Result<u64> {
        let idx_name = self.pg().index_sql_name(self.index_name());
        let row = self
            .pg()
            .client()
            .query_one(&format!("SELECT pg_relation_size('{idx_name}')"), &[])
            .await?;
        let size: i64 = row.get(0);
        Ok(size as u64)
    }

    async fn index_exists(&self) -> Result<bool> {
        let idx_name = self.pg().index_sql_name(self.index_name());
        let row = self
            .pg()
            .client()
            .query_one(
                "SELECT EXISTS(
                    SELECT 1 FROM pg_indexes
                    WHERE tablename = $1 AND indexname = $2
                )",
                &[&self.pg().table_name, &idx_name],
            )
            .await?;
        Ok(row.get(0))
    }

    async fn set_search_params(&self, params: &HashMap<String, toml::Value>) -> Result<()> {
        self.pg().set_gucs(params).await
    }

    async fn validate_search_params(
        &self,
        params: &HashMap<String, Vec<toml::Value>>,
    ) -> Result<()> {
        self.pg().validate_gucs(params).await
    }

    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>> {
        let k_i64 = k as i64;
        let stmt = self
            .custom_search_stmt()
            .unwrap_or_else(|| self.pg().query_stmt());
        let rows = self
            .pg()
            .client()
            .query(stmt, &[&PgVector(vector), &k_i64])
            .await?;
        Ok(rows.iter().map(|r| r.get::<_, i64>(0)).collect())
    }

    async fn validate_query_plan(&self, sample_vector: &[f32], k: usize) -> Result<()> {
        if self.custom_search_stmt().is_none() {
            self.pg()
                .validate_query_plan(self.index_name(), sample_vector, k)
                .await?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct BackendRegistry;

impl BackendRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub async fn create(
        &self,
        conn: &ConnectionConfig,
        table: &str,
        dimension: u32,
        metric: Metric,
        index_config: &IndexConfig,
        cancel_registry: Option<&crate::CancelRegistry>,
    ) -> Result<Box<dyn IndexBackend>> {
        match index_config.index_type.as_str() {
            "meerkat" => {
                let backend = meerkat::MeerkatBackend::create(
                    conn,
                    table,
                    dimension,
                    metric,
                    index_config,
                    cancel_registry,
                )
                .await?;
                Ok(Box::new(backend) as Box<dyn IndexBackend>)
            }
            "pgvector-hnsw" => {
                let backend = pgvector_hnsw::PgvectorHnswBackend::create(
                    conn,
                    table,
                    dimension,
                    metric,
                    index_config,
                    cancel_registry,
                )
                .await?;
                Ok(Box::new(backend) as Box<dyn IndexBackend>)
            }
            t @ ("vchordrq" | "vchordrq-rabitq8" | "vchordrq-rabitq4") => {
                let variant = match t {
                    "vchordrq" => vchordrq::Variant::Plain,
                    "vchordrq-rabitq8" => vchordrq::Variant::Rabitq8,
                    "vchordrq-rabitq4" => vchordrq::Variant::Rabitq4Rerank,
                    _ => unreachable!(),
                };
                let backend = vchordrq::VchordrqBackend::create(
                    conn,
                    table,
                    dimension,
                    metric,
                    index_config,
                    cancel_registry,
                    variant,
                )
                .await?;
                Ok(Box::new(backend) as Box<dyn IndexBackend>)
            }
            other => Err(Error::UnknownBackend {
                backend_type: other.to_string(),
            }),
        }
    }
}

/// Shared interface for data loading (PG backends share this via PgConnection).
#[async_trait]
pub trait DataLoader: Send + Sync {
    async fn table_exists(&self) -> Result<bool>;
    async fn row_count(&self) -> Result<u64>;
    async fn create_table(&self) -> Result<()>;
    async fn create_meta_tables(&self) -> Result<()>;

    /// Load vectors from a channel of chunks. The caller is responsible for
    /// streaming chunks from the dataset reader (in a blocking thread) into
    /// the provided channel receiver.
    async fn load_data(
        &self,
        total: u64,
        chunk_rx: &mut tokio::sync::mpsc::Receiver<Result<crate::dataset::VectorChunk>>,
        progress: &ProgressTx,
    ) -> Result<u64>;
}
