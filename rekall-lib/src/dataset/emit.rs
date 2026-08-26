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

/// A self-contained statement that joins the file's ground truth against
/// what the table actually returns.
///
/// The truth is inlined as a VALUES list, so the result needs nothing but
/// the table -- paste it into psql and read the verdict per neighbour.
///
/// The join is on **id**, not rank. Joining on rank only shows positional
/// agreement and leaves `missed`/`extra` unreachable, because a full outer
/// join over 1..k always pairs up. Joining on id answers the question that
/// matters -- which true neighbours came back -- so every status is live:
///
///   ok      a true neighbour was returned
///   missed  a true neighbour was not returned
///   tie     a returned row is not in the truth list but is at least as
///           good as the k-th true neighbour, within `epsilon`. This is the
///           same tolerance `recall_distance_based` applies; without it a
///           correct index looks broken wherever the data clusters.
///   extra   a returned row that is genuinely worse than the k-th
pub fn compare_sql(
    target: &SqlTarget,
    query: &[f32],
    truth_ids: &[i64],
    truth_dists: Option<&[f32]>,
    k: usize,
    epsilon: f64,
) -> String {
    let bound = target.bind(&vector_literal(query));
    let n = truth_ids.len().min(k);

    let values: Vec<String> = (0..n)
        .map(|i| match truth_dists {
            Some(d) => format!("({}, {}::bigint, {}::float8)", i + 1, truth_ids[i], d[i]),
            None => format!("({}, {}::bigint, NULL::float8)", i + 1, truth_ids[i]),
        })
        .collect();

    format!(
        "WITH truth(rank, id, distance) AS (\n  VALUES {values}\n),\n         got AS (\n  SELECT {id} AS id,\n         {operand} {op} {bound} AS distance,\n                  row_number() OVER (ORDER BY {operand} {op} {bound}) AS rank\n             FROM {table}\n   ORDER BY 2\n   LIMIT {k}\n),\n         kth AS (SELECT max(distance) AS d FROM truth)\n         SELECT t.rank AS truth_rank,\n       g.rank AS got_rank,\n                coalesce(t.id, g.id) AS id,\n                t.distance AS truth_distance,\n       g.distance AS got_distance,\n                CASE WHEN t.id IS NOT NULL AND g.id IS NOT NULL THEN 'ok'\n                     WHEN g.id IS NULL THEN 'missed'\n                     WHEN g.distance <= (SELECT d FROM kth) + {epsilon} THEN 'tie'\n                     ELSE 'extra' END AS status\n           FROM truth t FULL OUTER JOIN got g ON t.id = g.id\n          ORDER BY coalesce(t.rank, 2147483647), g.rank",
        values = values.join(",\n         "),
        id = target.id_column,
        operand = target.operand,
        op = target.order_op,
        bound = bound,
        table = target.table,
        k = k,
        epsilon = epsilon,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_sql_inlines_truth_and_marks_ties() {
        let t = SqlTarget::defaults("tbl", 2, Metric::Angular);
        let sql = compare_sql(&t, &[1.0, 0.0], &[7, 8], Some(&[0.1, 0.2]), 2, 1e-3);
        assert!(sql.contains("(1, 7::bigint, 0.1::float8)"));
        assert!(sql.contains("(2, 8::bigint, 0.2::float8)"));
        assert!(sql.contains("FROM tbl"));
        assert!(sql.contains("FULL OUTER JOIN got g ON t.id = g.id"));
        assert!(sql.contains("'missed'"));
        assert!(sql.contains("'extra'"));
        assert!(sql.contains("'tie'"));
        assert!(sql.contains("LIMIT 2"));
    }

    #[test]
    fn compare_sql_without_distances_emits_nulls() {
        let t = SqlTarget::defaults("tbl", 2, Metric::Euclidean);
        let sql = compare_sql(&t, &[1.0, 0.0], &[7], None, 1, 1e-3);
        assert!(sql.contains("(1, 7::bigint, NULL::float8)"));
        assert!(sql.contains(" <-> "));
    }

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
