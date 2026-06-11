//! Generate the plot reports for a run into its results directory, reusing the
//! shared `output::plot` renderer. HTML always works; SVG is best-effort (it
//! needs `gnuplot` on PATH) and is skipped with a warning if unavailable.

use std::path::{Path, PathBuf};

use crate::metrics::RunSummary;
use crate::output::plot::{generate_build_plot, generate_plot, PlotOptions};

/// Report kinds we always generate from a `RunSummary` (these need only the
/// summarized search results, not the dataset / ground truth).
const KINDS: &[(&str, &str, bool)] = &[
    ("pareto", "Recall vs QPS (Pareto)", false),
    ("per-config", "Recall vs QPS (per config)", true),
];

/// File stem prefix for a run's reports, matching `cmd_report`'s convention.
pub fn report_prefix(summary: &RunSummary) -> String {
    if summary.dataset.is_empty() {
        "report".to_string()
    } else {
        summary.dataset.clone()
    }
}

/// Generate the report plots into `dir`: pareto + per-config always, plus the
/// build-time chart (if builds) and distance-recall (if ground truth is
/// available, optionally via `dataset`). Returns the files written. Failures
/// (e.g. no gnuplot for SVG) are logged, not fatal — HTML is what Pages serves.
pub fn generate_into_dir(summary: &RunSummary, dir: &Path, dataset: Option<&Path>) -> Vec<PathBuf> {
    let prefix = report_prefix(summary);
    let mut written = Vec::new();
    for &(kind, title, per_config) in KINDS {
        for ext in ["html", "svg"] {
            let path = dir.join(format!("{prefix}-report-{kind}.{ext}"));
            let opts = PlotOptions {
                per_config,
                ..Default::default()
            };
            match generate_plot(std::slice::from_ref(summary), &path, Some(title), &opts) {
                Ok(()) => written.push(path),
                Err(e) => tracing::warn!("could not generate {}: {e}", path.display()),
            }
        }
    }

    // Distance-recall (ann-benchmarks method): needs per-query IDs + ground
    // truth with distances. Skipped cleanly when either is missing.
    if let Some(gt) = super::distance::load_ground_truth(dir, dataset, summary) {
        let results = super::distance::recompute(summary, &gt);
        if results.is_empty() {
            tracing::info!("distance-recall: no per-query results saved, skipping");
        } else {
            let mut dr = summary.clone();
            dr.search_results = results;
            for ext in ["html", "svg"] {
                let path = dir.join(format!("{prefix}-report-distance-recall.{ext}"));
                let opts = PlotOptions {
                    per_config: true,
                    ..Default::default()
                };
                match generate_plot(
                    std::slice::from_ref(&dr),
                    &path,
                    Some("Recall vs QPS (ann-benchmarks distance method)"),
                    &opts,
                ) {
                    Ok(()) => written.push(path),
                    Err(e) => tracing::warn!("could not generate {}: {e}", path.display()),
                }
            }
        }
    }

    // Build-time bar chart, when the run recorded index builds.
    if !summary.index_builds.is_empty() {
        for ext in ["html", "svg"] {
            let path = dir.join(format!("{prefix}-report-build-times.{ext}"));
            match generate_build_plot(
                &summary.index_builds,
                std::slice::from_ref(summary),
                &path,
                Some("Index build time"),
            ) {
                Ok(()) => written.push(path),
                Err(e) => tracing::warn!("could not generate {}: {e}", path.display()),
            }
        }
    }

    written
}
