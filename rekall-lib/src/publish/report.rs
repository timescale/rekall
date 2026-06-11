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

/// Generate `<dataset>-report-{pareto,per-config}.{html,svg}` into `dir`.
/// Returns the files that were written. Failures (e.g. no gnuplot for SVG)
/// are logged, not fatal — the HTML reports are what Pages serves.
pub fn generate_into_dir(summary: &RunSummary, dir: &Path) -> Vec<PathBuf> {
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
