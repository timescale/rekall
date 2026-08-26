//! Emit SQL and ground-truth listings from a dataset file, offline.
//!
//! Everything here is a pure function of the HDF5 contents plus a
//! [`SqlTarget`]. No database is contacted, which is the point: the SQL is
//! meant to be produced on a laptop and pasted into whichever server has the
//! data, and the ground truth is a property of the file rather than of any
//! table.

use crate::config::Metric;

/// Where the emitted SQL should point, and how.
///
/// Defaults mirror what a real `rekall` load produces, so the generated SQL
/// runs unmodified against a table rekall created: the DDL is
/// `(id bigint NOT NULL, v vector(N))` and the loader writes
/// `COPY t (id, v)`, hence the `id`/`v` names.
#[derive(Debug, Clone)]
pub struct SqlTarget {
    pub table: String,
    /// The ORDER BY operand. Usually the bare column, but an expression-index
    /// arm makes it an expression -- `(v::halfvec(768))` and the like -- which
    /// is why this is a String and not a column name.
    pub operand: String,
    pub id_column: String,
    pub dimension: u32,
    /// The distance operator: `<=>`, `<->`, `<~>`, or a schema-qualified
    /// `OPERATOR(mkt.<=>)`.
    pub order_op: String,
    /// SQL rendering the query vector, given the literal. Lets a quantized
    /// arm wrap it in the cast its operator needs.
    pub bind_template: BindTemplate,
}

/// How the query vector is spliced into the SQL.
#[derive(Debug, Clone)]
pub enum BindTemplate {
    /// `'[...]'::vector(N)` — the plain pgvector form.
    Vector,
    /// An arm-specific cast with `{}` marking where the literal goes, e.g.
    /// `"({}::vector(768))::halfvec(768)"`.
    Wrapped(String),
}

impl SqlTarget {
    /// The rekall-default target for a dataset: table `name` with `-`
    /// replaced by `_`, columns `id`/`v`, operator from the metric.
    pub fn defaults(table: impl Into<String>, dimension: u32, metric: Metric) -> Self {
        Self {
            table: table.into(),
            operand: "v".to_string(),
            id_column: "id".to_string(),
            dimension,
            order_op: default_order_op(metric).to_string(),
            bind_template: BindTemplate::Vector,
        }
    }

    fn bind(&self, literal: &str) -> String {
        match &self.bind_template {
            BindTemplate::Vector => format!("{literal}::vector({})", self.dimension),
            BindTemplate::Wrapped(t) => t.replace("{}", literal),
        }
    }

    /// `SELECT id FROM t ORDER BY <operand> <op> <bound vector> LIMIT k`
    pub fn select(&self, query: &[f32], k: usize) -> String {
        let bound = self.bind(&vector_literal(query));
        format!(
            "SELECT {} FROM {} ORDER BY {} {} {} LIMIT {}",
            self.id_column, self.table, self.operand, self.order_op, bound, k
        )
    }

    /// The same, with the distance projected alongside the id — what a
    /// comparison against ground-truth distances needs.
    pub fn select_with_distance(&self, query: &[f32], k: usize) -> String {
        let bound = self.bind(&vector_literal(query));
        format!(
            "SELECT {id}, {operand} {op} {bound} AS distance\n  FROM {table}\n \
             ORDER BY {operand} {op} {bound}\n LIMIT {k}",
            id = self.id_column,
            operand = self.operand,
            op = self.order_op,
            bound = bound,
            table = self.table,
            k = k,
        )
    }
}

/// Operator for a metric under the plain `vector` encoding. Mirrors
/// `Encoding::order_op`; quantized arms override it via the tailored path.
pub fn default_order_op(metric: Metric) -> &'static str {
    match metric {
        Metric::Angular => "<=>",
        Metric::Euclidean => "<->",
    }
}

/// A pgvector vector literal: `'[1,2,3]'`.
///
/// Uses `{}` float formatting, which round-trips f32 shortest-exactly, so
/// the emitted text parses back to the same vector the file holds.
pub fn vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 12 + 4);
    s.push('\'');
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s.push('\'');
    s
}

/// Infer the metric from an ann-benchmarks `distance` attribute.
///
/// Returns None rather than guessing: the metric picks the operator, and a
/// wrong operator yields SQL that runs fine and silently returns the wrong
/// rows. A loud failure is better than a plausible one.
pub fn metric_from_attr(attr: &str) -> Option<Metric> {
    match attr.trim().to_ascii_lowercase().as_str() {
        "angular" | "cosine" | "dot" | "ip" | "inner_product" => Some(Metric::Angular),
        "euclidean" | "l2" | "l2sq" => Some(Metric::Euclidean),
        _ => None,
    }
}

/// Last-resort metric inference from a filename (ann-benchmarks names carry
/// the metric as a suffix, e.g. `...-1M-angular.hdf5`).
pub fn metric_from_filename(name: &str) -> Option<Metric> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("angular") || lower.contains("cosine") || lower.contains("dot") {
        Some(Metric::Angular)
    } else if lower.contains("euclidean") || lower.contains("-l2") {
        Some(Metric::Euclidean)
    } else {
        None
    }
}

/// rekall's table-name rule, applied to a bare filename stem.
///
/// This will NOT match a table loaded from a suite: the config names the
/// dataset (`cohere-1m` -> `cohere_1m`) and nothing in the file recovers
/// that. Callers should say where the name came from.
pub fn table_from_stem(stem: &str) -> String {
    stem.replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_literal_round_trips() {
        assert_eq!(vector_literal(&[1.0, -2.5, 0.25]), "'[1,-2.5,0.25]'");
    }

    #[test]
    fn select_uses_defaults() {
        let t = SqlTarget::defaults("cohere_1m", 3, Metric::Angular);
        assert_eq!(
            t.select(&[1.0, 2.0, 3.0], 10),
            "SELECT id FROM cohere_1m ORDER BY v <=> '[1,2,3]'::vector(3) LIMIT 10"
        );
    }

    #[test]
    fn euclidean_picks_the_l2_operator() {
        let t = SqlTarget::defaults("t", 2, Metric::Euclidean);
        assert!(t.select(&[0.0, 0.0], 1).contains(" <-> "));
    }

    #[test]
    fn wrapped_bind_applies_the_arms_cast() {
        let mut t = SqlTarget::defaults("t", 4, Metric::Angular);
        t.operand = "(v::halfvec(4))".to_string();
        t.bind_template = BindTemplate::Wrapped("({}::vector(4))::halfvec(4)".to_string());
        let sql = t.select(&[1.0, 2.0, 3.0, 4.0], 5);
        assert!(sql.contains("ORDER BY (v::halfvec(4)) <=> ('[1,2,3,4]'::vector(4))::halfvec(4)"));
    }

    #[test]
    fn metric_inference_refuses_the_unknown() {
        assert_eq!(metric_from_attr("angular"), Some(Metric::Angular));
        assert_eq!(metric_from_attr("euclidean"), Some(Metric::Euclidean));
        assert_eq!(metric_from_attr("hamming"), None);
        assert_eq!(
            metric_from_filename("cohere-1M-angular.hdf5"),
            Some(Metric::Angular)
        );
        assert_eq!(metric_from_filename("mystery.hdf5"), None);
    }

    #[test]
    fn table_name_follows_rekalls_rule() {
        assert_eq!(table_from_stem("cohere-1m"), "cohere_1m");
    }
}
