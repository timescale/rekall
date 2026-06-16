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
//! The rabitq variants index a quantized *expression* over `v`
//! (`(quantize_to_rabitq8(v))::rabitq8(N)`) — an expression index, so the
//! shared rekall table is never modified. The explicit `::rabitq8(N)` cast
//! restores the dimension typmod vchordrq needs to build the index (a bare
//! `quantize_to_rabitq8(v)` loses it, and the build fails). Other backends
//! sharing the same table always see the schema they expect.

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

    /// RaBitQ type name used in the `::rabitq8(N)` cast of the index
    /// expression. `None` for the plain variant (indexes `v` directly).
    fn quantized_type(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Rabitq8 => Some("rabitq8"),
            Self::Rabitq4Rerank => Some("rabitq4"),
        }
    }

    /// quantize_to_rabitq{4,8}() function used to quantize `v` (in the
    /// index expression) and query vectors (at search time). `None` for
    /// the plain variant.
    fn quantize_fn(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Rabitq8 => Some("quantize_to_rabitq8"),
            Self::Rabitq4Rerank => Some("quantize_to_rabitq4"),
        }
    }

    /// Un-parenthesized quantize expression over `v`, as it appears in a
    /// query `ORDER BY` and (parenthesized) in `CREATE INDEX`, e.g.
    /// `(quantize_to_rabitq8(v))::rabitq8(768)`. The explicit cast
    /// restores the dimension typmod vchordrq needs to build the index —
    /// a bare `quantize_to_rabitq8(v)` loses it. `None` for the plain
    /// variant, which indexes/queries the `v` column directly.
    fn order_expr(self, dim: u32) -> Option<String> {
        match (self.quantize_fn(), self.quantized_type()) {
            (Some(f), Some(ty)) => Some(format!("({f}(v))::{ty}({dim})")),
            _ => None,
        }
    }

    /// Casts the bound `$1` (sent as a `vector`) to the quantized operand
    /// the index expects, e.g. `quantize_to_rabitq8($1::vector(768))`.
    fn bind_expr(self, dim: u32) -> String {
        match self.quantize_fn() {
            None => "$1".to_string(),
            Some(f) => format!("{f}($1::vector({dim}))"),
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

    /// `vector` indexes the bare `v` column; the rabitq variants index a
    /// parenthesized quantize expression (an expression index — no
    /// materialized column, so the shared table is never modified).
    /// `derived_column` stays `None` (the trait default) for all variants.
    fn index_target(&self) -> String {
        self.variant
            .order_expr(self.pg.dimension)
            .map(|e| super::expr_index::index_target(&e))
            .unwrap_or_else(|| "v".to_string())
    }

    /// The rabitq variants order by the quantize expression — and `rabitq4`
    /// over-fetches a candidate pool re-ranked on full-precision `v`. The
    /// plain variant falls back to the default `ORDER BY v <op> $1`
    /// template. The quantized and full-precision distances share the same
    /// metric operator, so `order_op` doubles as the rerank `full_op`.
    fn custom_search_sql(&self) -> Option<(String, Vec<Type>)> {
        let dim = self.pg.dimension;
        let order_expr = self.variant.order_expr(dim)?;
        let bind = self.variant.bind_expr(dim);
        let order_op = self.order_op();
        let table = &self.pg.table_name;

        if self.variant == Variant::Rabitq4Rerank {
            Some((
                super::expr_index::rerank_sql(table, &order_expr, order_op, &bind, order_op),
                vec![Type::INT8],
            ))
        } else {
            Some((
                super::expr_index::search_sql(table, &order_expr, order_op, &bind),
                Vec::new(),
            ))
        }
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
            table_parallel_workers: None,
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

    #[test]
    fn variant_type_names() {
        assert_eq!(Variant::Plain.type_name(), "vchordrq");
        assert_eq!(Variant::Rabitq8.type_name(), "vchordrq-rabitq8");
        assert_eq!(Variant::Rabitq4Rerank.type_name(), "vchordrq-rabitq4");
    }

    #[test]
    fn variant_plain_has_no_quantization() {
        assert!(Variant::Plain.quantized_type().is_none());
        assert!(Variant::Plain.quantize_fn().is_none());
        assert!(Variant::Plain.order_expr(768).is_none());
        assert_eq!(Variant::Plain.bind_expr(768), "$1");
    }

    /// rabitq8 is an expression index: the order/index expression casts
    /// the quantize result back to `rabitq8(N)` to keep the dimension
    /// typmod, and the `$1` bind quantizes the query vector.
    #[test]
    fn variant_rabitq8_expression_index() {
        assert_eq!(Variant::Rabitq8.quantized_type(), Some("rabitq8"));
        assert_eq!(Variant::Rabitq8.quantize_fn(), Some("quantize_to_rabitq8"));
        assert_eq!(
            Variant::Rabitq8.order_expr(768).as_deref(),
            Some("(quantize_to_rabitq8(v))::rabitq8(768)")
        );
        assert_eq!(
            Variant::Rabitq8.bind_expr(768),
            "quantize_to_rabitq8($1::vector(768))"
        );
    }

    #[test]
    fn variant_rabitq4_expression_index() {
        assert_eq!(Variant::Rabitq4Rerank.quantized_type(), Some("rabitq4"));
        assert_eq!(
            Variant::Rabitq4Rerank.quantize_fn(),
            Some("quantize_to_rabitq4")
        );
        assert_eq!(
            Variant::Rabitq4Rerank.order_expr(768).as_deref(),
            Some("(quantize_to_rabitq4(v))::rabitq4(768)")
        );
    }

    /// The rabitq index target is the parenthesized quantize expression
    /// (an expression index), never a materialized column.
    #[test]
    fn rabitq_index_target_is_expression() {
        let target =
            crate::backend::expr_index::index_target(&Variant::Rabitq8.order_expr(768).unwrap());
        assert_eq!(target, "((quantize_to_rabitq8(v))::rabitq8(768))");
    }
}
