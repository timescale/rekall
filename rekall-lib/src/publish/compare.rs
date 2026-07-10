//! Comparison logic behind the published summary tables and the catalog's
//! Meerkat-vs-competition column. Pure data → data; rendering lives in
//! `pages.rs`.
//!
//! Two grouping modes, chosen by what a run contains:
//! - **Families** (≥2 index families, e.g. Meerkat vs pgvector HNSW): a
//!   family's value at a recall target is the best any of its configs
//!   achieved — the tables compare algorithms, not configs.
//! - **Configs** (single-family run, e.g. a Meerkat parameter sweep): one
//!   column per index config, no Meerkat-vs-competitor delta.

use std::collections::BTreeMap;

use crate::metrics::family_label;

use super::summary::{BuildInfo, CompactSummary, SweepPoint};

/// Recall targets the run-page tables report at.
pub const RECALL_TARGETS: &[f64] = &[0.90, 0.95, 0.99];

/// The catalog comparison ignores frontier overlap below this recall —
/// deltas at recall 0.5 say nothing about practical operating points.
pub const CATALOG_MIN_RECALL: f64 = 0.80;

pub fn is_meerkat(index_type: &str) -> bool {
    index_type == "meerkat"
}

fn point_family(p: &SweepPoint) -> String {
    family_label(&p.index_type, p.data_type.as_deref())
}

/// Group points by family label (deterministic order).
pub fn group_by_family(points: &[SweepPoint]) -> BTreeMap<String, Vec<&SweepPoint>> {
    let mut out: BTreeMap<String, Vec<&SweepPoint>> = BTreeMap::new();
    for p in points {
        out.entry(point_family(p)).or_default().push(p);
    }
    out
}

/// Group points by index config name (deterministic order).
pub fn group_by_config(points: &[SweepPoint]) -> BTreeMap<String, Vec<&SweepPoint>> {
    let mut out: BTreeMap<String, Vec<&SweepPoint>> = BTreeMap::new();
    for p in points {
        out.entry(p.index_name.clone()).or_default().push(p);
    }
    out
}

/// The operating point at a recall target: the highest-QPS point with
/// recall ≥ target. `None` when no point reaches the target.
pub fn operating_point<'a>(points: &[&'a SweepPoint], target: f64) -> Option<&'a SweepPoint> {
    points
        .iter()
        .filter(|p| p.recall >= target)
        .max_by(|a, b| a.qps.total_cmp(&b.qps))
        .copied()
}

pub fn max_recall(points: &[&SweepPoint]) -> Option<f64> {
    points
        .iter()
        .map(|p| p.recall)
        .max_by(f64::total_cmp)
        .filter(|r| r.is_finite())
}

/// (recall, qps) Pareto frontier, recall-ascending, QPS strictly descending —
/// the points no other point beats on both axes.
pub fn pareto_frontier(points: &[&SweepPoint]) -> Vec<(f64, f64)> {
    let mut pts: Vec<(f64, f64)> = points.iter().map(|p| (p.recall, p.qps)).collect();
    // Sort by recall descending; sweep keeping a running max QPS. A point is
    // on the frontier iff its QPS beats everything at equal-or-higher recall.
    pts.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.total_cmp(&a.1)));
    let mut frontier = Vec::new();
    let mut best_qps = f64::NEG_INFINITY;
    for (r, q) in pts {
        if q > best_qps {
            frontier.push((r, q));
            best_qps = q;
        }
    }
    frontier.reverse();
    frontier
}

/// QPS on a frontier at an exact recall, interpolating linearly in
/// (recall, ln QPS) between neighbouring frontier points. `None` outside the
/// frontier's recall range — no extrapolation.
pub fn qps_at(frontier: &[(f64, f64)], recall: f64) -> Option<f64> {
    let first = frontier.first()?;
    let last = frontier.last()?;
    if recall < first.0 || recall > last.0 {
        return None;
    }
    // Find the segment [lo, hi] bracketing the recall; frontier points
    // themselves are returned exactly (no ln/exp round-trip error).
    for w in frontier.windows(2) {
        let (r0, q0) = w[0];
        let (r1, q1) = w[1];
        if recall == r0 {
            return Some(q0);
        }
        if recall == r1 {
            return Some(q1);
        }
        if recall > r0 && recall < r1 {
            let t = (recall - r0) / (r1 - r0);
            return Some((q0.ln() + t * (q1.ln() - q0.ln())).exp());
        }
    }
    // recall == the single frontier point's recall.
    (recall == first.0).then_some(first.1)
}

/// One catalog comparison sample: Meerkat's QPS gain/loss at an exact recall.
#[derive(Debug, Clone, PartialEq)]
pub struct DeltaSample {
    pub recall: f64,
    /// (meerkat_qps / competitor_qps − 1) · 100.
    pub pct: f64,
}

/// Meerkat vs one competitor family across their common recall range.
#[derive(Debug, Clone)]
pub struct CompetitorDelta {
    pub competitor: String,
    /// Samples at the low / middle / high recall of the common frontier
    /// range (clamped to ≥ `CATALOG_MIN_RECALL` when possible). Empty when
    /// the frontiers don't overlap.
    pub samples: Vec<DeltaSample>,
}

/// The catalog's Meerkat-vs-competition summary. Meerkat = the pooled points
/// of every `meerkat` family (configs and variants collapse into one
/// frontier); one entry per competitor family. Empty when the run has no
/// meerkat points or no competitors.
pub fn meerkat_comparison(cs: &CompactSummary) -> Vec<CompetitorDelta> {
    let (mkt_points, rest): (Vec<&SweepPoint>, Vec<&SweepPoint>) =
        cs.points.iter().partition(|p| is_meerkat(&p.index_type));
    if mkt_points.is_empty() {
        return Vec::new();
    }
    let mkt = pareto_frontier(&mkt_points);

    let mut out = Vec::new();
    let mut competitors: BTreeMap<String, Vec<&SweepPoint>> = BTreeMap::new();
    for p in rest {
        competitors.entry(point_family(p)).or_default().push(p);
    }
    for (name, points) in competitors {
        let comp = pareto_frontier(&points);
        out.push(CompetitorDelta {
            competitor: name,
            samples: delta_samples(&mkt, &comp),
        });
    }
    out
}

fn delta_samples(mkt: &[(f64, f64)], comp: &[(f64, f64)]) -> Vec<DeltaSample> {
    let (Some(&(m_lo, _)), Some(&(m_hi, _))) = (mkt.first(), mkt.last()) else {
        return Vec::new();
    };
    let (Some(&(c_lo, _)), Some(&(c_hi, _))) = (comp.first(), comp.last()) else {
        return Vec::new();
    };
    let lo = m_lo.max(c_lo);
    let hi = m_hi.min(c_hi);
    if lo > hi {
        return Vec::new();
    }
    // Prefer the practical range; fall back to the raw overlap when the
    // frontiers never meet above the clamp.
    let lo = if lo.max(CATALOG_MIN_RECALL) <= hi {
        lo.max(CATALOG_MIN_RECALL)
    } else {
        lo
    };

    let mut recalls = vec![lo, (lo + hi) / 2.0, hi];
    recalls.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    recalls
        .into_iter()
        .filter_map(|r| {
            let m = qps_at(mkt, r)?;
            let c = qps_at(comp, r)?;
            Some(DeltaSample {
                recall: r,
                pct: (m / c - 1.0) * 100.0,
            })
        })
        .collect()
}

/// Column grouping of the run-page tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnMode {
    Families,
    Configs,
}

/// One column's operating point at one recall target.
#[derive(Debug, Clone)]
pub struct OperatingCell {
    /// The config that produced the point (footnote material; equals the
    /// column name in `Configs` mode).
    pub index_name: String,
    pub recall: f64,
    pub qps: f64,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
}

#[derive(Debug, Clone)]
pub enum Cell {
    Reached(OperatingCell),
    /// Column never reaches the target; its best recall for "— (max r=…)".
    Unreached {
        max_recall: f64,
    },
}

/// One recall target's row: cells parallel to `RunTableModel::columns`.
#[derive(Debug, Clone)]
pub struct TargetRow {
    pub target: f64,
    pub cells: Vec<Cell>,
}

#[derive(Debug, Clone)]
pub struct BuildCell {
    pub index_name: String,
    pub build_time_secs: f64,
    pub size_bytes: u64,
    pub cache: Option<String>,
}

/// Everything the run-page tables need, with all selection logic resolved.
#[derive(Debug, Clone)]
pub struct RunTableModel {
    pub mode: ColumnMode,
    pub columns: Vec<String>,
    /// Column index of the (single, pooled) meerkat family — `Some` only in
    /// `Families` mode with at least one competitor, i.e. when the tables
    /// carry a Δ column.
    pub meerkat_col: Option<usize>,
    /// Targets at which at least one column has a reached cell.
    pub rows: Vec<TargetRow>,
    /// Per column: the build backing its operating point at the highest
    /// target it reaches (its best config's build in `Families` mode).
    /// `None` renders "—" (e.g. search-only reruns without build files).
    pub build_cells: Vec<Option<BuildCell>>,
}

/// Build the table model. `None` only when the run has no search points.
pub fn build_table_model(cs: &CompactSummary, targets: &[f64]) -> Option<RunTableModel> {
    if cs.points.is_empty() {
        return None;
    }
    let families = group_by_family(&cs.points);
    let (mode, groups) = if families.len() >= 2 {
        (ColumnMode::Families, families)
    } else {
        (ColumnMode::Configs, group_by_config(&cs.points))
    };

    // Meerkat first, then competitors ordered by QPS at the lowest target
    // (strongest competitor next to Meerkat); groups that reach no target
    // keep their BTreeMap order at the end.
    let lowest = targets.iter().copied().fold(f64::INFINITY, f64::min);
    let mut names: Vec<&String> = groups.keys().collect();
    if mode == ColumnMode::Families {
        let strength = |name: &String| {
            operating_point(&groups[name], lowest)
                .map(|p| p.qps)
                .unwrap_or(0.0)
        };
        names.sort_by(|a, b| {
            let a_mkt = groups[*a].iter().any(|p| is_meerkat(&p.index_type));
            let b_mkt = groups[*b].iter().any(|p| is_meerkat(&p.index_type));
            b_mkt.cmp(&a_mkt).then(strength(b).total_cmp(&strength(a)))
        });
    }
    let columns: Vec<String> = names.iter().map(|n| n.to_string()).collect();

    let meerkat_col = if mode == ColumnMode::Families && columns.len() >= 2 {
        columns
            .iter()
            .position(|c| groups[c].iter().any(|p| is_meerkat(&p.index_type)))
    } else {
        None
    };

    let mut rows = Vec::new();
    for &target in targets {
        let cells: Vec<Cell> = columns
            .iter()
            .map(|c| match operating_point(&groups[c], target) {
                Some(p) => Cell::Reached(OperatingCell {
                    index_name: p.index_name.clone(),
                    recall: p.recall,
                    qps: p.qps,
                    p50_us: p.p50_latency_us,
                    p95_us: p.p95_latency_us,
                    p99_us: p.p99_latency_us,
                }),
                None => Cell::Unreached {
                    max_recall: max_recall(&groups[c]).unwrap_or(0.0),
                },
            })
            .collect();
        if cells.iter().any(|c| matches!(c, Cell::Reached(_))) {
            rows.push(TargetRow { target, cells });
        }
    }

    // Build cells: the build behind each column's best operating point.
    let builds: BTreeMap<&str, &BuildInfo> = cs
        .builds
        .iter()
        .map(|b| (b.index_name.as_str(), b))
        .collect();
    let build_cells: Vec<Option<BuildCell>> = columns
        .iter()
        .map(|c| {
            // The config at the highest reached target, else the max-recall
            // point's config (a column with points always has one).
            let config = targets
                .iter()
                .rev()
                .find_map(|&t| operating_point(&groups[c], t))
                .map(|p| p.index_name.clone())
                .or_else(|| {
                    let mut pts = groups[c].clone();
                    pts.sort_by(|a, b| b.recall.total_cmp(&a.recall));
                    pts.first().map(|p| p.index_name.clone())
                })?;
            builds.get(config.as_str()).map(|b| BuildCell {
                index_name: config.clone(),
                build_time_secs: b.build_time_secs,
                size_bytes: b.size_bytes,
                cache: b.cache.clone(),
            })
        })
        .collect();

    Some(RunTableModel {
        mode,
        columns,
        meerkat_col,
        rows,
        build_cells,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publish::summary::RecallBasis;

    fn point(name: &str, index_type: &str, recall: f64, qps: f64) -> SweepPoint {
        SweepPoint {
            index_name: name.into(),
            index_type: index_type.into(),
            data_type: None,
            recall,
            qps,
            p50_latency_us: 1e6 / qps * 0.9,
            p95_latency_us: 1e6 / qps * 1.5,
            p99_latency_us: 1e6 / qps * 2.0,
        }
    }

    fn build(name: &str, index_type: &str, secs: f64, bytes: u64) -> BuildInfo {
        BuildInfo {
            index_name: name.into(),
            index_type: index_type.into(),
            build_time_secs: secs,
            size_bytes: bytes,
            cache: None,
        }
    }

    fn summary(points: Vec<SweepPoint>, builds: Vec<BuildInfo>) -> CompactSummary {
        CompactSummary {
            recall_basis: RecallBasis::Distance,
            points,
            builds,
        }
    }

    fn refs(points: &[SweepPoint]) -> Vec<&SweepPoint> {
        points.iter().collect()
    }

    #[test]
    fn operating_point_is_highest_qps_reaching_target() {
        let pts = vec![
            point("a", "meerkat", 0.89, 5000.0),
            point("a", "meerkat", 0.92, 3000.0),
            point("b", "meerkat", 0.91, 3500.0),
            point("a", "meerkat", 0.97, 1000.0),
        ];
        let op = operating_point(&refs(&pts), 0.90).unwrap();
        assert_eq!(op.index_name, "b"); // 3500 beats 3000 among recall >= 0.90
        assert_eq!(op.recall, 0.91);
        // Exact boundary counts.
        let pts2 = vec![point("c", "meerkat", 0.95, 100.0)];
        assert!(operating_point(&refs(&pts2), 0.95).is_some());
        // Unreached.
        assert!(operating_point(&refs(&pts), 0.99).is_none());
        assert_eq!(max_recall(&refs(&pts)), Some(0.97));
    }

    #[test]
    fn pareto_frontier_removes_dominated_points() {
        let pts = vec![
            point("a", "meerkat", 0.90, 1000.0),
            point("a", "meerkat", 0.92, 1200.0), // dominates the 0.90 point
            point("a", "meerkat", 0.95, 800.0),
            point("a", "meerkat", 0.94, 700.0), // dominated
        ];
        let f = pareto_frontier(&refs(&pts));
        assert_eq!(f, vec![(0.92, 1200.0), (0.95, 800.0)]);
    }

    #[test]
    fn multiple_meerkat_configs_collapse_into_one_frontier() {
        let pts = vec![
            point("mkt-n8000", "meerkat", 0.90, 2000.0),
            point("mkt-n16000", "meerkat", 0.90, 2500.0),
            point("mkt-n8000", "meerkat", 0.96, 900.0),
        ];
        let f = pareto_frontier(&refs(&pts));
        assert_eq!(f, vec![(0.90, 2500.0), (0.96, 900.0)]);
    }

    #[test]
    fn qps_at_interpolates_geometrically() {
        let f = vec![(0.90, 100.0), (0.94, 400.0)];
        let q = qps_at(&f, 0.92).unwrap();
        assert!((q - 200.0).abs() < 1e-9, "expected geometric mean, got {q}");
        assert_eq!(qps_at(&f, 0.90), Some(100.0));
        assert_eq!(qps_at(&f, 0.94), Some(400.0));
        assert_eq!(qps_at(&f, 0.89), None);
        assert_eq!(qps_at(&f, 0.95), None);
        // Single-point frontier: only the exact recall matches.
        let single = vec![(0.93, 500.0)];
        assert_eq!(qps_at(&single, 0.93), Some(500.0));
        assert_eq!(qps_at(&single, 0.92), None);
    }

    #[test]
    fn meerkat_comparison_samples_common_range() {
        let cs = summary(
            vec![
                point("mkt", "meerkat", 0.85, 4000.0),
                point("mkt", "meerkat", 0.95, 2000.0),
                point("hnsw", "pgvector-hnsw", 0.85, 2000.0),
                point("hnsw", "pgvector-hnsw", 0.95, 1000.0),
            ],
            vec![],
        );
        let deltas = meerkat_comparison(&cs);
        assert_eq!(deltas.len(), 1);
        let d = &deltas[0];
        assert_eq!(d.competitor, "pgvector HNSW");
        assert_eq!(d.samples.len(), 3);
        // Meerkat is 2x everywhere -> +100% at every sample.
        for s in &d.samples {
            assert!((s.pct - 100.0).abs() < 1.0, "pct {}", s.pct);
        }
        assert_eq!(d.samples[0].recall, 0.85);
        assert_eq!(d.samples[2].recall, 0.95);
        assert!((d.samples[1].recall - 0.90).abs() < 1e-9);
    }

    #[test]
    fn meerkat_comparison_clamps_low_recall() {
        let cs = summary(
            vec![
                point("mkt", "meerkat", 0.50, 9000.0),
                point("mkt", "meerkat", 0.95, 2000.0),
                point("hnsw", "pgvector-hnsw", 0.50, 5000.0),
                point("hnsw", "pgvector-hnsw", 0.95, 1000.0),
            ],
            vec![],
        );
        let d = &meerkat_comparison(&cs)[0];
        assert!((d.samples[0].recall - CATALOG_MIN_RECALL).abs() < 1e-9);
    }

    #[test]
    fn meerkat_comparison_falls_back_below_clamp() {
        // Overlap exists only below 0.80 — fall back to the raw overlap.
        let cs = summary(
            vec![
                point("mkt", "meerkat", 0.55, 9000.0),
                point("mkt", "meerkat", 0.75, 4000.0),
                point("hnsw", "pgvector-hnsw", 0.60, 5000.0),
                point("hnsw", "pgvector-hnsw", 0.78, 2000.0),
            ],
            vec![],
        );
        let d = &meerkat_comparison(&cs)[0];
        assert!(!d.samples.is_empty());
        assert!((d.samples[0].recall - 0.60).abs() < 1e-9);
        assert!((d.samples.last().unwrap().recall - 0.75).abs() < 1e-9);
    }

    #[test]
    fn meerkat_comparison_empty_cases() {
        // No overlap at all.
        let cs = summary(
            vec![
                point("mkt", "meerkat", 0.95, 2000.0),
                point("mkt", "meerkat", 0.99, 500.0),
                point("hnsw", "pgvector-hnsw", 0.80, 5000.0),
                point("hnsw", "pgvector-hnsw", 0.90, 2000.0),
            ],
            vec![],
        );
        let deltas = meerkat_comparison(&cs);
        assert_eq!(deltas.len(), 1);
        assert!(deltas[0].samples.is_empty());

        // No meerkat family.
        let cs = summary(vec![point("hnsw", "pgvector-hnsw", 0.9, 1000.0)], vec![]);
        assert!(meerkat_comparison(&cs).is_empty());

        // Meerkat-only run (no competitors).
        let cs = summary(vec![point("mkt", "meerkat", 0.9, 1000.0)], vec![]);
        assert!(meerkat_comparison(&cs).is_empty());
    }

    #[test]
    fn table_model_families_mode_with_delta() {
        let cs = summary(
            vec![
                point("mkt-a", "meerkat", 0.91, 3000.0),
                point("mkt-b", "meerkat", 0.96, 1500.0),
                point("hnsw", "pgvector-hnsw", 0.91, 2000.0),
                point("hnsw", "pgvector-hnsw", 0.94, 1200.0),
            ],
            vec![
                build("mkt-b", "meerkat", 300.0, 3_000_000_000),
                build("hnsw", "pgvector-hnsw", 500.0, 5_000_000_000),
            ],
        );
        let m = build_table_model(&cs, RECALL_TARGETS).unwrap();
        assert_eq!(m.mode, ColumnMode::Families);
        assert_eq!(m.columns[0], "Meerkat"); // meerkat first
        assert_eq!(m.meerkat_col, Some(0));
        // 0.99 reached by nobody -> row dropped; 0.90 and 0.95 remain.
        assert_eq!(m.rows.len(), 2);
        assert_eq!(m.rows[0].target, 0.90);
        assert_eq!(m.rows[1].target, 0.95);
        // At 0.95 the competitor is unreached with its max recall noted.
        match &m.rows[1].cells[1] {
            Cell::Unreached { max_recall } => assert_eq!(*max_recall, 0.94),
            _ => panic!("expected unreached"),
        }
        // Build cells: meerkat column backed by mkt-b (highest reached
        // target 0.95), competitor by hnsw.
        assert_eq!(m.build_cells[0].as_ref().unwrap().index_name, "mkt-b");
        assert_eq!(m.build_cells[1].as_ref().unwrap().index_name, "hnsw");
    }

    #[test]
    fn table_model_configs_mode_without_delta() {
        let cs = summary(
            vec![
                point("mkt-serial", "meerkat", 0.91, 2000.0),
                point("mkt-parallel", "meerkat", 0.92, 2100.0),
            ],
            vec![build("mkt-serial", "meerkat", 900.0, 1_000_000)],
        );
        let m = build_table_model(&cs, RECALL_TARGETS).unwrap();
        assert_eq!(m.mode, ColumnMode::Configs);
        assert!(m.meerkat_col.is_none());
        assert_eq!(m.columns.len(), 2);
        // Config without a build record -> None ("—"), never a guess.
        let parallel_col = m.columns.iter().position(|c| c == "mkt-parallel").unwrap();
        assert!(m.build_cells[parallel_col].is_none());
        let serial_col = m.columns.iter().position(|c| c == "mkt-serial").unwrap();
        assert!(m.build_cells[serial_col].is_some());
    }

    #[test]
    fn table_model_single_config_still_renders() {
        let cs = summary(vec![point("mkt", "meerkat", 0.91, 2000.0)], vec![]);
        let m = build_table_model(&cs, RECALL_TARGETS).unwrap();
        assert_eq!(m.mode, ColumnMode::Configs);
        assert_eq!(m.columns, vec!["mkt".to_string()]);
        assert_eq!(m.rows.len(), 1); // only the 0.90 target is reached
        assert!(m.meerkat_col.is_none());
    }

    #[test]
    fn table_model_no_points_is_none() {
        let cs = summary(vec![], vec![]);
        assert!(build_table_model(&cs, RECALL_TARGETS).is_none());
    }

    #[test]
    fn table_model_orders_competitors_by_strength() {
        let cs = summary(
            vec![
                point("mkt", "meerkat", 0.91, 3000.0),
                point("weak", "vchordrq", 0.91, 500.0),
                point("strong", "pgvector-hnsw", 0.91, 2000.0),
            ],
            vec![],
        );
        let m = build_table_model(&cs, RECALL_TARGETS).unwrap();
        assert_eq!(
            m.columns,
            vec!["Meerkat", "pgvector HNSW", "vchordrq"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }
}
