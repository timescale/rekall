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
    matches!(index_type, "prism" | "meerkat")
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

/// The catalog's Meerkat-vs-competition summary. PRISM = every `meerkat`
/// point regardless of config/variant; one entry per competitor family.
/// Values are interpolated per config sweep and the best line wins (never
/// blended across configs). Empty when the run has no meerkat points or no
/// competitors.
pub fn meerkat_comparison(cs: &CompactSummary) -> Vec<CompetitorDelta> {
    let (mkt_points, rest): (Vec<&SweepPoint>, Vec<&SweepPoint>) =
        cs.points.iter().partition(|p| is_meerkat(&p.index_type));
    if mkt_points.is_empty() {
        return Vec::new();
    }
    let mkt = config_lines(&mkt_points);

    let mut out = Vec::new();
    let mut competitors: BTreeMap<String, Vec<&SweepPoint>> = BTreeMap::new();
    for p in rest {
        competitors.entry(point_family(p)).or_default().push(p);
    }
    for (name, points) in competitors {
        let comp = config_lines(&points);
        out.push(CompetitorDelta {
            competitor: name,
            samples: delta_samples(&mkt, &comp),
        });
    }
    out
}

/// A family's QPS at an exact recall: the best of its configs' own
/// interpolated sweeps; `None` when no single sweep spans the recall.
fn best_line_qps(lines: &[Vec<&SweepPoint>], recall: f64) -> Option<f64> {
    lines
        .iter()
        .filter_map(|l| match cell_at(l, recall) {
            Cell::Interpolated(c) => Some(c.qps),
            _ => None,
        })
        .max_by(f64::total_cmp)
}

fn recall_span(lines: &[Vec<&SweepPoint>]) -> Option<(f64, f64)> {
    let lo = lines
        .iter()
        .filter_map(|l| l.first())
        .map(|p| p.recall)
        .min_by(f64::total_cmp)?;
    let hi = lines
        .iter()
        .filter_map(|l| l.last())
        .map(|p| p.recall)
        .max_by(f64::total_cmp)?;
    Some((lo, hi))
}

fn delta_samples(mkt: &[Vec<&SweepPoint>], comp: &[Vec<&SweepPoint>]) -> Vec<DeltaSample> {
    let (Some((m_lo, m_hi)), Some((c_lo, c_hi))) = (recall_span(mkt), recall_span(comp)) else {
        return Vec::new();
    };
    let lo = m_lo.max(c_lo);
    let hi = m_hi.min(c_hi);
    if lo > hi {
        return Vec::new();
    }
    // Prefer the practical range; fall back to the raw overlap when the
    // sweeps never meet above the clamp.
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
            // Skips samples landing in a coverage gap on either side.
            let m = best_line_qps(mkt, r)?;
            let c = best_line_qps(comp, r)?;
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

/// One column's numbers at exactly one recall target, log-interpolated
/// between the two surrounding points of a SINGLE config's sweep (taken
/// verbatim when a measured point lands exactly on it). Interpolating both
/// QPS and the latency percentiles at the same recall makes every column —
/// and therefore every Δ — comparable at the identical recall. Values are
/// never blended across configs: two curves bracketing a recall say nothing
/// about either curve's value there.
#[derive(Debug, Clone)]
pub struct InterpCell {
    /// The config whose sweep the value was interpolated on.
    pub config: String,
    pub qps: f64,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
}

#[derive(Debug, Clone)]
pub enum Cell {
    /// Some config's sweep spans the target; the best such config's value.
    Interpolated(InterpCell),
    /// Every measured point sits below the target ("— (max r=…)").
    Below { max_recall: f64 },
    /// All sweeps start above the target — nothing was measured that low,
    /// and we never extrapolate ("— (min r=…)").
    Above { min_recall: f64 },
    /// The column's sweeps surround the target but no single sweep spans it.
    Gap,
}

fn log_interp(v0: f64, v1: f64, t: f64) -> f64 {
    if v0 <= 0.0 || v1 <= 0.0 {
        // Degenerate recorded value; fall back to linear rather than NaN.
        return v0 + t * (v1 - v0);
    }
    (v0.ln() + t * (v1.ln() - v0.ln())).exp()
}

/// Pareto frontier as full sweep points, recall-ascending / QPS-descending.
pub fn frontier_points<'a>(points: &[&'a SweepPoint]) -> Vec<&'a SweepPoint> {
    let mut pts: Vec<&SweepPoint> = points.to_vec();
    pts.sort_by(|a, b| b.recall.total_cmp(&a.recall).then(b.qps.total_cmp(&a.qps)));
    let mut frontier: Vec<&SweepPoint> = Vec::new();
    let mut best_qps = f64::NEG_INFINITY;
    for p in pts {
        if p.qps > best_qps {
            frontier.push(p);
            best_qps = p.qps;
        }
    }
    frontier.reverse();
    frontier
}

fn exact_cell(p: &SweepPoint) -> InterpCell {
    InterpCell {
        config: p.index_name.clone(),
        qps: p.qps,
        p50_us: p.p50_latency_us,
        p95_us: p.p95_latency_us,
        p99_us: p.p99_latency_us,
    }
}

/// One config's sweep at exactly `target`. `line` must be a single config's
/// points, Pareto-filtered and recall-ascending (`config_lines`).
pub fn cell_at(line: &[&SweepPoint], target: f64) -> Cell {
    let (Some(first), Some(last)) = (line.first(), line.last()) else {
        return Cell::Below { max_recall: 0.0 };
    };
    if target > last.recall {
        return Cell::Below {
            max_recall: last.recall,
        };
    }
    if target < first.recall {
        return Cell::Above {
            min_recall: first.recall,
        };
    }
    if let Some(p) = line.iter().find(|p| p.recall == target) {
        return Cell::Interpolated(exact_cell(p));
    }
    for w in line.windows(2) {
        let (a, b) = (w[0], w[1]);
        if target > a.recall && target < b.recall {
            let t = (target - a.recall) / (b.recall - a.recall);
            return Cell::Interpolated(InterpCell {
                config: a.index_name.clone(),
                qps: log_interp(a.qps, b.qps, t),
                p50_us: log_interp(a.p50_latency_us, b.p50_latency_us, t),
                p95_us: log_interp(a.p95_latency_us, b.p95_latency_us, t),
                p99_us: log_interp(a.p99_latency_us, b.p99_latency_us, t),
            });
        }
    }
    // Unreachable given the range checks above; be safe.
    Cell::Below {
        max_recall: last.recall,
    }
}

/// A column's points split into per-config "lines", each Pareto-filtered and
/// recall-ascending — the unit interpolation is allowed to operate on.
pub fn config_lines<'a>(points: &[&'a SweepPoint]) -> Vec<Vec<&'a SweepPoint>> {
    let mut by_config: BTreeMap<&str, Vec<&SweepPoint>> = BTreeMap::new();
    for p in points {
        by_config.entry(p.index_name.as_str()).or_default().push(p);
    }
    by_config
        .into_values()
        .map(|v| frontier_points(&v))
        .collect()
}

/// A column's cell at exactly `target`: interpolate each config's own sweep
/// and take the best (highest-QPS) line. Never blends across configs — a
/// bracketing point from a much worse run would drag the value down without
/// saying anything about the better run's curve.
pub fn cell_at_best_line(lines: &[Vec<&SweepPoint>], target: f64) -> Cell {
    let mut best: Option<InterpCell> = None;
    for line in lines {
        if let Cell::Interpolated(c) = cell_at(line, target) {
            if best.as_ref().is_none_or(|b| c.qps > b.qps) {
                best = Some(c);
            }
        }
    }
    if let Some(c) = best {
        return Cell::Interpolated(c);
    }
    let max = lines
        .iter()
        .filter_map(|l| l.last())
        .map(|p| p.recall)
        .fold(f64::NEG_INFINITY, f64::max);
    let min = lines
        .iter()
        .filter_map(|l| l.first())
        .map(|p| p.recall)
        .fold(f64::INFINITY, f64::min);
    if max == f64::NEG_INFINITY {
        Cell::Below { max_recall: 0.0 }
    } else if target > max {
        Cell::Below { max_recall: max }
    } else if target < min {
        Cell::Above { min_recall: min }
    } else {
        Cell::Gap
    }
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
    /// Targets at which at least one column has an interpolated cell.
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

    let lines: Vec<Vec<Vec<&SweepPoint>>> =
        columns.iter().map(|c| config_lines(&groups[c])).collect();

    let mut rows = Vec::new();
    for &target in targets {
        let cells: Vec<Cell> = lines.iter().map(|l| cell_at_best_line(l, target)).collect();
        if cells.iter().any(|c| matches!(c, Cell::Interpolated(_))) {
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
    }

    #[test]
    fn frontier_points_removes_dominated_points() {
        let pts = vec![
            point("a", "meerkat", 0.90, 1000.0),
            point("a", "meerkat", 0.92, 1200.0), // dominates the 0.90 point
            point("a", "meerkat", 0.95, 800.0),
            point("a", "meerkat", 0.94, 700.0), // dominated
        ];
        let f = frontier_points(&refs(&pts));
        let pairs: Vec<(f64, f64)> = f.iter().map(|p| (p.recall, p.qps)).collect();
        assert_eq!(pairs, vec![(0.92, 1200.0), (0.95, 800.0)]);
    }

    #[test]
    fn best_line_wins_and_lines_never_blend() {
        // Config mkt-a's own sweep spans 0.95; mkt-b contributes a single
        // point at 0.94 that would sit on the pooled frontier between
        // mkt-a's points. Blending mkt-b's point into the interpolation
        // (the old pooled-frontier behaviour) would yield a misleading
        // value; the cell must come from mkt-a's own curve alone.
        let pts = vec![
            point("mkt-a", "meerkat", 0.90, 3000.0),
            point("mkt-a", "meerkat", 0.96, 1500.0),
            point("mkt-b", "meerkat", 0.94, 2000.0),
        ];
        let lines = config_lines(&refs(&pts));
        let Cell::Interpolated(c) = cell_at_best_line(&lines, 0.95) else {
            panic!("expected interpolated");
        };
        assert_eq!(c.config, "mkt-a");
        let expected = 3000.0 * (1500.0_f64 / 3000.0).powf(5.0 / 6.0);
        assert!((c.qps - expected).abs() < 1e-6, "got {}", c.qps);

        // At 0.94, mkt-b's measured point (2000) beats mkt-a's
        // interpolation (~1888) — the best line changes per target.
        let Cell::Interpolated(c) = cell_at_best_line(&lines, 0.94) else {
            panic!("expected interpolated");
        };
        assert_eq!(c.config, "mkt-b");
        assert_eq!(c.qps, 2000.0);
    }

    #[test]
    fn best_line_gap_and_direction_annotations() {
        // Two single-point sweeps surround 0.95 but neither spans it.
        let pts = vec![
            point("a", "meerkat", 0.90, 3000.0),
            point("b", "meerkat", 0.96, 1500.0),
        ];
        let lines = config_lines(&refs(&pts));
        assert!(matches!(cell_at_best_line(&lines, 0.95), Cell::Gap));
        match cell_at_best_line(&lines, 0.99) {
            Cell::Below { max_recall } => assert_eq!(max_recall, 0.96),
            _ => panic!("expected below"),
        }
        match cell_at_best_line(&lines, 0.85) {
            Cell::Above { min_recall } => assert_eq!(min_recall, 0.90),
            _ => panic!("expected above"),
        }
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
    fn cell_at_interpolates_all_metrics_between_surrounding_points() {
        let pts = vec![
            point("a", "meerkat", 0.90, 400.0),
            point("a", "meerkat", 0.94, 100.0),
        ];
        let f = frontier_points(&refs(&pts));
        // Midpoint: geometric mean of the surrounding values, for QPS and
        // for every latency percentile alike.
        let Cell::Interpolated(c) = cell_at(&f, 0.92) else {
            panic!("expected interpolated");
        };
        assert!((c.qps - 200.0).abs() < 1e-9);
        let (p50_a, p50_b) = (pts[0].p50_latency_us, pts[1].p50_latency_us);
        assert!((c.p50_us - (p50_a * p50_b).sqrt()).abs() < 1e-9);
        assert_eq!(c.config, "a");
        // Exact hit: measured values verbatim.
        let Cell::Interpolated(c) = cell_at(&f, 0.94) else {
            panic!("expected exact hit");
        };
        assert_eq!(c.qps, 100.0);
        assert_eq!(c.config, "a");
        // Outside the frontier: no extrapolation, direction annotated.
        match cell_at(&f, 0.89) {
            Cell::Above { min_recall } => assert_eq!(min_recall, 0.90),
            _ => panic!("expected above"),
        }
        match cell_at(&f, 0.95) {
            Cell::Below { max_recall } => assert_eq!(max_recall, 0.94),
            _ => panic!("expected below"),
        }
    }

    #[test]
    fn table_model_families_mode_with_delta() {
        let cs = summary(
            vec![
                point("mkt-a", "meerkat", 0.90, 3000.0),
                point("mkt-a", "meerkat", 0.96, 1500.0),
                point("hnsw", "pgvector-hnsw", 0.90, 2000.0),
                point("hnsw", "pgvector-hnsw", 0.94, 1200.0),
            ],
            vec![
                build("mkt-a", "meerkat", 300.0, 3_000_000_000),
                build("hnsw", "pgvector-hnsw", 500.0, 5_000_000_000),
            ],
        );
        let m = build_table_model(&cs, RECALL_TARGETS).unwrap();
        assert_eq!(m.mode, ColumnMode::Families);
        assert_eq!(m.columns[0], "PRISM"); // meerkat first
        assert_eq!(m.meerkat_col, Some(0));
        // 0.99 spanned by nobody -> row dropped; 0.90 and 0.95 remain.
        assert_eq!(m.rows.len(), 2);
        assert_eq!(m.rows[0].target, 0.90);
        assert_eq!(m.rows[1].target, 0.95);
        // Exact hits at 0.90 keep the measured values.
        match &m.rows[0].cells[0] {
            Cell::Interpolated(c) => assert_eq!(c.qps, 3000.0),
            _ => panic!("expected interpolated"),
        }
        // At 0.95 meerkat interpolates within mkt-a's own sweep; the
        // competitor's sweep tops out below the target.
        match &m.rows[1].cells[0] {
            Cell::Interpolated(c) => {
                assert!(c.qps < 3000.0 && c.qps > 1500.0);
                assert_eq!(c.config, "mkt-a");
            }
            _ => panic!("expected interpolated"),
        }
        match &m.rows[1].cells[1] {
            Cell::Below { max_recall } => assert_eq!(*max_recall, 0.94),
            _ => panic!("expected below"),
        }
        // Build cells: meerkat column backed by mkt-a (highest reached
        // target 0.95), competitor by hnsw.
        assert_eq!(m.build_cells[0].as_ref().unwrap().index_name, "mkt-a");
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
        let cs = summary(
            vec![
                point("mkt", "meerkat", 0.88, 2500.0),
                point("mkt", "meerkat", 0.92, 1800.0),
            ],
            vec![],
        );
        let m = build_table_model(&cs, RECALL_TARGETS).unwrap();
        assert_eq!(m.mode, ColumnMode::Configs);
        assert_eq!(m.columns, vec!["mkt".to_string()]);
        assert_eq!(m.rows.len(), 1); // only 0.90 is spanned by the sweep
        assert!(m.meerkat_col.is_none());
    }

    #[test]
    fn table_model_empty_rows_when_no_target_spanned() {
        // A single measured point spans no target: model still built (build
        // table can render), but no QPS/latency rows.
        let cs = summary(vec![point("mkt", "meerkat", 0.91, 2000.0)], vec![]);
        let m = build_table_model(&cs, RECALL_TARGETS).unwrap();
        assert!(m.rows.is_empty());
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
            vec!["PRISM", "pgvector HNSW", "vchordrq"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }
}
