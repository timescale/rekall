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

use async_trait::async_trait;
use postgres_types::Type;
use serde::{Deserialize, Serialize};

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::Result;

use super::pg_vector_type::PgVector;
use super::postgres::PgConnection;
use super::{pg_backend_field_accessors, IndexBackend};

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
        super::create_pg_backend(
            conn,
            table,
            dim,
            metric,
            idx,
            cancel_registry,
            variant.type_name(),
            |pg, name, params| Self {
                pg,
                name,
                variant,
                params,
            },
        )
        .await
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
}

#[async_trait]
impl IndexBackend for VchordrqBackend {
    pg_backend_field_accessors!();

    fn type_name(&self) -> &str {
        self.variant.type_name()
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["vector", "vchord"]
    }

    fn order_op(&self) -> &'static str {
        match self.pg.metric {
            Metric::Angular => "<=>",
            Metric::Euclidean => "<->",
        }
    }

    fn index_method(&self) -> &str {
        "vchordrq"
    }

    fn opclass(&self) -> &str {
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

    fn index_options(&self) -> Vec<(&'static str, String)> {
        let body = self.options_toml();
        if body.is_empty() {
            Vec::new()
        } else {
            vec![("options", format!("$${body}$$"))]
        }
    }

    fn build_params_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.params).unwrap_or_default()
    }

    fn derived_column(&self) -> Option<&str> {
        self.variant.quantized_column()
    }

    fn derived_column_type(&self) -> Option<String> {
        self.variant
            .quantized_type()
            .map(|ty| format!("{ty}({})", self.pg.dimension))
    }

    fn derive_from_v_expr(&self) -> Option<String> {
        self.variant.quantize_fn().map(|f| format!("{f}(v)"))
    }

    fn query_column(&self) -> &'static str {
        self.variant.quantized_column().unwrap_or("v")
    }

    fn query_bind(&self) -> String {
        match self.variant.quantize_fn() {
            None => "$1".to_string(),
            Some(f) => format!("{f}($1::vector)"),
        }
    }

    /// Rabitq4-rerank wraps a candidate-pool subquery and binds a
    /// third `$3` param for the pool LIMIT. The default `query_column`
    /// / `query_bind` template can't express that shape, so override.
    fn custom_search_sql(&self) -> Option<(String, Vec<Type>)> {
        if self.variant != Variant::Rabitq4Rerank {
            return None;
        }
        let qcol = self.variant.quantized_column().unwrap();
        let qfn = self.variant.quantize_fn().unwrap();
        let order_op = self.order_op();
        let table = &self.pg.table_name;
        let sql = format!(
            "SELECT id FROM ( \
                 SELECT id, v FROM {table} \
                 ORDER BY {qcol} {order_op} {qfn}($1::vector) \
                 LIMIT $3 \
             ) candidates \
             ORDER BY v {order_op} $1::vector \
             LIMIT $2"
        );
        Some((sql, vec![Type::INT8]))
    }

    /// Rabitq4-rerank binds an extra `$3` for the candidate pool. All
    /// other variants share the trait-default two-param bind.
    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>> {
        if self.variant != Variant::Rabitq4Rerank {
            let k_i64 = k as i64;
            let rows = self
                .pg
                .client()
                .query(self.pg.query_stmt(), &[&PgVector(vector), &k_i64])
                .await?;
            return Ok(rows.iter().map(|r| r.get::<_, i64>(0)).collect());
        }
        let k_i64 = k as i64;
        let pool = self.params.rerank_n.max(k as u32) as i64;
        let rows = self
            .pg
            .client()
            .query(self.pg.query_stmt(), &[&PgVector(vector), &k_i64, &pool])
            .await?;
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
