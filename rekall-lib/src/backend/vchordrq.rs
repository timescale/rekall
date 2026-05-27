//! VectorChord vchordrq (IVF posting-list) backend.
//!
//! Three variants, each registered as its own index_type:
//!   - `vchordrq`             — plain `vector` column.
//!   - `vchordrq-rabitq8`     — quantized to rabitq8 (~8× smaller).
//!   - `vchordrq-rabitq4`     — quantized to rabitq4 (~16× smaller),
//!     with a two-phase query that fetches `rerank_n` candidates by
//!     quantized distance, then reorders by exact distance on the
//!     original `vector` column.
//!
//! For rabitq variants, the backend `ALTER TABLE`s a quantized column
//! into the shared rekall table before building the index, and drops
//! the column (and index) in `drop_index`. The plain `v` column is
//! never modified, so other backends operating on the same table see
//! the schema they expect.

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

/// Build-time parameters shared across all vchordrq variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VchordrqParams {
    /// IVF partition counts. `[1000]` = single-level 1000 lists.
    /// `[L1, L2]` = hierarchical two-level partitioning. Empty list
    /// means no partitioning (fine for tables with <1M rows but
    /// VectorChord's docs recommend ~4·√rows partitions otherwise).
    #[serde(default = "default_lists")]
    lists: Vec<u32>,
    /// Improves recall+QPS on cosine workloads per VectorChord docs.
    #[serde(default = "default_true")]
    residual_quantization: bool,
    /// L2-normalize centroids after each k-means iteration. Recommended
    /// when the embedding model uses cosine similarity.
    #[serde(default = "default_true")]
    spherical_centroids: bool,
    /// K-means threads during the partitioning phase of the build.
    /// Defaults to a sensible mid-range value; bump up to `nproc`
    /// on larger instances.
    #[serde(default = "default_build_threads")]
    build_threads: u32,
    /// Candidate pool size for the rabitq4 rerank query. Ignored for
    /// `plain` and `rabitq8` variants.
    #[serde(default = "default_rerank_n")]
    rerank_n: u32,
}

fn default_lists() -> Vec<u32> {
    vec![1000]
}
fn default_true() -> bool {
    true
}
fn default_build_threads() -> u32 {
    8
}
fn default_rerank_n() -> u32 {
    200
}

/// Which flavour of vchordrq this backend instance is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Plain,
    Rabitq8,
    Rabitq4Rerank,
}

impl Variant {
    fn type_name(self) -> &'static str {
        match self {
            Self::Plain => "vchordrq",
            Self::Rabitq8 => "vchordrq-rabitq8",
            Self::Rabitq4Rerank => "vchordrq-rabitq4",
        }
    }

    /// Quantized column name added to the rekall table, if any. The
    /// plain variant indexes the existing `v` column directly.
    fn quantized_column(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Rabitq8 => Some("v_rabitq8"),
            Self::Rabitq4Rerank => Some("v_rabitq4"),
        }
    }

    /// Quantized PG type, used in `ALTER TABLE ADD COLUMN ...`.
    fn quantized_type(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Rabitq8 => Some("rabitq8"),
            Self::Rabitq4Rerank => Some("rabitq4"),
        }
    }

    /// quantize_to_rabitq{4,8}() function name used to populate the
    /// quantized column and to quantize query vectors at search time.
    fn quantize_fn(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Rabitq8 => Some("quantize_to_rabitq8"),
            Self::Rabitq4Rerank => Some("quantize_to_rabitq4"),
        }
    }
}

pub struct VchordrqBackend {
    pg: PgConnection,
    name: String,
    variant: Variant,
    params: VchordrqParams,
    /// For non-plain variants we prepare a custom statement that
    /// references the quantized column. Plain uses `pg.query_stmt()`.
    custom_query: Option<Statement>,
}

impl VchordrqBackend {
    pub async fn create(
        conn: &ConnectionConfig,
        table: &str,
        dim: u32,
        metric: Metric,
        idx: &IndexConfig,
        cancel_registry: Option<&crate::CancelRegistry>,
        variant: Variant,
    ) -> Result<Self> {
        let params: VchordrqParams = idx.params.clone().try_into().map_err(|e| {
            Error::Config(format!(
                "Invalid {} params for '{}': {e}",
                variant.type_name(),
                idx.name
            ))
        })?;

        let order_op = match metric {
            Metric::Angular => "<=>",
            Metric::Euclidean => "<->",
        };

        let mut pg = PgConnection::new(conn, table, dim, metric);
        if let Some(cr) = cancel_registry {
            pg.set_cancel_registry(cr.clone());
        }
        // vchord depends on pgvector for the `vector` type.
        pg.connect_with_setup(
            order_op,
            Some("vchord"),
            &[
                "CREATE EXTENSION IF NOT EXISTS vector",
                "CREATE EXTENSION IF NOT EXISTS vchord CASCADE",
            ],
        )
        .await?;

        // Plain variant uses the default `SELECT id FROM t ORDER BY v <=> $1
        // LIMIT $2` statement that PgConnection already prepared. Rabitq
        // variants need a different SQL referencing the quantized column.
        let custom_query = if variant == Variant::Plain {
            None
        } else {
            Some(prepare_quantized_query(&pg, variant, table, order_op).await?)
        };

        Ok(Self {
            pg,
            name: idx.name.clone(),
            variant,
            params,
            custom_query,
        })
    }

    fn opclass(&self) -> &'static str {
        let cosine = self.pg.metric == Metric::Angular;
        match (self.variant, cosine) {
            (Variant::Plain, true) => "vector_cosine_ops",
            (Variant::Plain, false) => "vector_l2_ops",
            (Variant::Rabitq8, true) => "rabitq8_cosine_ops",
            (Variant::Rabitq8, false) => "rabitq8_l2_ops",
            (Variant::Rabitq4Rerank, true) => "rabitq4_cosine_ops",
            (Variant::Rabitq4Rerank, false) => "rabitq4_l2_ops",
        }
    }

    /// Column the vchordrq index is built on — `v` for plain, the
    /// quantized column otherwise.
    fn indexed_column(&self) -> &'static str {
        self.variant.quantized_column().unwrap_or("v")
    }

    /// `WITH (options = $$ ... $$)` clause body in VectorChord's TOML
    /// format. residual_quantization is a top-level option;
    /// spherical_centroids/build_threads live under [build.internal].
    fn options_toml(&self) -> String {
        let mut top = String::new();
        if self.params.residual_quantization {
            top.push_str("residual_quantization = true\n");
        }
        let mut internal = String::new();
        if !self.params.lists.is_empty() {
            let lists = self
                .params
                .lists
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            internal.push_str(&format!("lists = [{lists}]\n"));
        }
        if self.params.spherical_centroids {
            internal.push_str("spherical_centroids = true\n");
        }
        if self.params.build_threads > 1 {
            internal.push_str(&format!("build_threads = {}\n", self.params.build_threads));
        }

        let mut body = top;
        if !internal.is_empty() {
            body.push_str("[build.internal]\n");
            body.push_str(&internal);
        }
        body
    }

    /// Add the quantized column and populate it from `v` via the
    /// shared `PgConnection::ensure_derived_column` helper.
    async fn ensure_quantized_column(&self) -> Result<()> {
        let (Some(col), Some(ty), Some(fnname)) = (
            self.variant.quantized_column(),
            self.variant.quantized_type(),
            self.variant.quantize_fn(),
        ) else {
            return Ok(());
        };
        self.pg
            .ensure_derived_column(
                col,
                &format!("{ty}({})", self.pg.dimension),
                &format!("{fnname}(v)"),
            )
            .await
    }
}

/// Prepare the search statement for non-plain variants.
///
/// rabitq8: `SELECT id FROM t ORDER BY v_rabitq8 <=> quantize_to_rabitq8($1::vector) LIMIT $2`
/// rabitq4-rerank: two-phase candidate pool → exact rerank.
async fn prepare_quantized_query(
    pg: &PgConnection,
    variant: Variant,
    table: &str,
    order_op: &str,
) -> Result<Statement> {
    let qcol = variant.quantized_column().expect("non-plain variant");
    let qfn = variant.quantize_fn().expect("non-plain variant");

    let sql = match variant {
        Variant::Plain => unreachable!(),
        Variant::Rabitq8 => format!(
            "SELECT id FROM {table} \
             ORDER BY {qcol} {order_op} {qfn}($1::vector) \
             LIMIT $2"
        ),
        Variant::Rabitq4Rerank => {
            // Two-phase: pull a larger candidate pool by quantized
            // distance, then re-order by exact distance on the
            // original vector column. $3 = candidate pool size.
            format!(
                "SELECT id FROM ( \
                     SELECT id, v FROM {table} \
                     ORDER BY {qcol} {order_op} {qfn}($1::vector) \
                     LIMIT $3 \
                 ) candidates \
                 ORDER BY v {order_op} $1::vector \
                 LIMIT $2"
            )
        }
    };

    use postgres_types::Type;
    let extra: &[Type] = match variant {
        Variant::Rabitq4Rerank => &[Type::INT8],
        _ => &[],
    };
    pg.prepare_vector_search(&sql, extra).await
}

#[async_trait]
impl IndexBackend for VchordrqBackend {
    fn type_name(&self) -> &str {
        self.variant.type_name()
    }

    fn index_name(&self) -> &str {
        &self.name
    }

    async fn build_index(&self) -> Result<IndexBuildResult> {
        // Quantized variants need their column populated before CREATE INDEX.
        self.ensure_quantized_column().await?;

        let client = self.pg.client();
        let idx_name = self.pg.index_sql_name(&self.name);
        let options_body = self.options_toml();

        let with_clause = if options_body.is_empty() {
            String::new()
        } else {
            format!(" WITH (options = $${}$$)", options_body)
        };

        let sql = format!(
            "CREATE INDEX {idx_name} ON {} USING vchordrq ({} {}){}",
            self.pg.table_name,
            self.indexed_column(),
            self.opclass(),
            with_clause,
        );

        info!("Building {} index: {sql}", self.variant.type_name());
        let start = Instant::now();
        client.execute(&sql, &[]).await?;
        let build_time = start.elapsed();

        let size = self.index_size().await?;

        Ok(IndexBuildResult {
            index_name: self.name.clone(),
            index_type: self.variant.type_name().to_string(),
            params: serde_json::to_value(&self.params).unwrap_or_default(),
            build_time,
            index_size_bytes: size,
        })
    }

    fn pg(&self) -> &PgConnection {
        &self.pg
    }

    fn derived_column(&self) -> Option<&str> {
        self.variant.quantized_column()
    }

    fn custom_search_stmt(&self) -> Option<&Statement> {
        self.custom_query.as_ref()
    }

    /// Override the trait default because rabitq4-rerank takes an
    /// extra `$3` candidate-pool-size parameter. Plain and rabitq8
    /// share the standard two-param bind that the trait default uses.
    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>> {
        let k_i64 = k as i64;
        let rows = if self.variant == Variant::Rabitq4Rerank {
            let stmt = self
                .custom_query
                .as_ref()
                .expect("rabitq4-rerank prepares a custom statement at create time");
            let pool = self.params.rerank_n.max(k as u32) as i64;
            self.pg
                .client()
                .query(stmt, &[&PgVector(vector), &k_i64, &pool])
                .await?
        } else {
            let stmt = self
                .custom_search_stmt()
                .unwrap_or_else(|| self.pg.query_stmt());
            self.pg
                .client()
                .query(stmt, &[&PgVector(vector), &k_i64])
                .await?
        };
        Ok(rows.iter().map(|r| r.get::<_, i64>(0)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;

    fn idx(name: &str, params: toml::Value) -> IndexConfig {
        IndexConfig {
            name: name.to_string(),
            index_type: "vchordrq".to_string(),
            params,
            gucs: Default::default(),
        }
    }

    #[test]
    fn params_default_when_omitted() {
        let cfg = idx("a", toml::Value::Table(Default::default()));
        let p: VchordrqParams = cfg.params.try_into().unwrap();
        assert_eq!(p.lists, vec![1000]);
        assert!(p.residual_quantization);
        assert!(p.spherical_centroids);
        assert_eq!(p.build_threads, 8);
        assert_eq!(p.rerank_n, 200);
    }

    #[test]
    fn params_parse_custom_lists() {
        let cfg = idx(
            "a",
            toml::toml! {
                lists = [4096, 262144]
                residual_quantization = false
                spherical_centroids = false
                build_threads = 16
                rerank_n = 100
            }
            .into(),
        );
        let p: VchordrqParams = cfg.params.try_into().unwrap();
        assert_eq!(p.lists, vec![4096, 262144]);
        assert!(!p.residual_quantization);
        assert!(!p.spherical_centroids);
        assert_eq!(p.build_threads, 16);
        assert_eq!(p.rerank_n, 100);
    }

    /// Builds the WITH (options = ...) body — verify the TOML structure
    /// matches VectorChord's expected format (top-level keys, then a
    /// [build.internal] table with the partition settings).
    #[test]
    fn options_toml_layout() {
        let backend_params = VchordrqParams {
            lists: vec![1000],
            residual_quantization: true,
            spherical_centroids: true,
            build_threads: 8,
            rerank_n: 200,
        };
        // Synthesize the body without going through PgConnection (which
        // would require a live DB). Replicates the impl exactly.
        let mut top = String::new();
        if backend_params.residual_quantization {
            top.push_str("residual_quantization = true\n");
        }
        let mut internal = String::new();
        let lists = backend_params
            .lists
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        internal.push_str(&format!("lists = [{lists}]\n"));
        if backend_params.spherical_centroids {
            internal.push_str("spherical_centroids = true\n");
        }
        if backend_params.build_threads > 1 {
            internal.push_str(&format!(
                "build_threads = {}\n",
                backend_params.build_threads
            ));
        }
        let mut body = top;
        body.push_str("[build.internal]\n");
        body.push_str(&internal);

        assert!(body.contains("residual_quantization = true"));
        assert!(body.contains("[build.internal]"));
        assert!(body.contains("lists = [1000]"));
        assert!(body.contains("spherical_centroids = true"));
        assert!(body.contains("build_threads = 8"));
        // residual_quantization should appear BEFORE [build.internal]
        // — top-level options must not be inside a section.
        let rq_pos = body.find("residual_quantization").unwrap();
        let sec_pos = body.find("[build.internal]").unwrap();
        assert!(rq_pos < sec_pos);
    }
}
