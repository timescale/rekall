use std::collections::BTreeMap;
use std::path::Path;

use crate::metrics::{IndexBuildResult, RunSummary, SearchResult};

mod build;
mod gnuplot;
mod html;

pub struct PlotOptions {
    pub log_x: bool,
    pub log_y: bool,
    pub pareto_only: bool,
    pub per_config: bool,
    pub min_recall: f64,
    pub use_wall_qps: bool,
}

impl Default for PlotOptions {
    fn default() -> Self {
        Self {
            log_x: true,
            log_y: true,
            pareto_only: false,
            per_config: false,
            min_recall: 0.0,
            use_wall_qps: false,
        }
    }
}

pub struct PlotData {
    pub title: String,
    /// Optional metadata sub-title (dataset, run date, machine, PG settings,
    /// extension versions) rendered under the main title. One logical line
    /// per `\n`. `None` when the source results carry no run metadata.
    pub caption: Option<String>,
    pub series: Vec<PlotSeries>,
}

pub struct PlotSeries {
    pub name: String,
    /// Pareto frontier points for this series (always shown as the main line)
    pub pareto_points: Vec<(f64, f64)>,
    /// Non-Pareto points (only shown with --per-config)
    pub other_points: Vec<(f64, f64)>,
}

const COLORS: &[(u8, u8, u8)] = &[
    (31, 119, 180),  // blue
    (255, 127, 14),  // orange
    (44, 160, 44),   // green
    (214, 39, 40),   // red
    (148, 103, 189), // purple
    (140, 86, 75),   // brown
    (227, 119, 194), // pink
    (127, 127, 127), // gray
    (188, 189, 34),  // olive
    (23, 190, 207),  // cyan
];

pub fn generate_plot(
    summaries: &[RunSummary],
    output_path: &Path,
    title: Option<&str>,
    opts: &PlotOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut data = prepare_plot_data(summaries, title.unwrap_or("Recall vs QPS"), opts);
    if data.series.is_empty() {
        return Err("No search results to plot".into());
    }
    data.caption = build_caption(summaries);

    let ext = output_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("html");

    match ext {
        "html" | "htm" => html::render(output_path, &data, opts),
        "gnuplot" | "gp" => gnuplot::render(output_path, None, &data, opts),
        "svg" | "png" => {
            let gp_path = output_path.with_extension("gnuplot");
            gnuplot::render(&gp_path, Some(output_path), &data, opts)?;
            run_gnuplot(&gp_path, output_path);
            Ok(())
        }
        _ => Err(format!("Unsupported format: {ext}. Use .html, .svg, .png, or .gnuplot").into()),
    }
}

/// Run gnuplot on `gp_path` to produce `output_path`; on success, post-process
/// SVG text. Best-effort: a missing/failing gnuplot prints a hint and leaves
/// the `.gnuplot` script for manual rendering (HTML reports are unaffected).
fn run_gnuplot(gp_path: &Path, output_path: &Path) {
    let is_svg = output_path.extension().and_then(|e| e.to_str()) == Some("svg");
    match std::process::Command::new("gnuplot").arg(gp_path).status() {
        Ok(s) if s.success() => {
            if is_svg {
                fix_svg_text(output_path);
            }
        }
        _ => eprintln!(
            "gnuplot not found or failed. Run manually:\n  gnuplot {}",
            gp_path.display()
        ),
    }
}

/// Render an index build-time bar chart (one bar per index, tallest first) to
/// `.html`, `.svg`, `.png`, or `.gnuplot`. `summaries` supplies the metadata
/// caption (same as the recall plots). Errors if there are no builds.
pub fn generate_build_plot(
    builds: &[IndexBuildResult],
    summaries: &[RunSummary],
    output_path: &Path,
    title: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if builds.is_empty() {
        return Err("No index builds to plot".into());
    }
    let base_title = title.unwrap_or("Index build time");
    // State the cache conditions in the title, derived from what each build
    // actually recorded — regenerated reports for old runs must not claim
    // conditions that were not controlled at the time.
    let labels: Vec<Option<&str>> = builds.iter().map(|b| b.cache.as_deref()).collect();
    let uniform = labels
        .first()
        .copied()
        .flatten()
        .filter(|l| labels.iter().all(|x| x.as_deref() == Some(*l)));
    let suffix: String = match uniform {
        Some("prewarmed") => " (heap prewarmed before each build)".into(),
        Some("cold") => {
            " (shared buffers evicted before each build; OS page cache untouched)".into()
        }
        Some("disk-cold") => {
            " (shared buffers and OS page caches dropped before each build)".into()
        }
        // Uniform but unrecognized (e.g. "cold (OS cache drop failed)"):
        // print it verbatim rather than guessing.
        Some(l) => format!(" (cache before each build: {l})"),
        None if labels.iter().all(|l| l.is_none()) => {
            " (cache state not controlled; build order determined warmth)".into()
        }
        None => " (mixed cache conditions across builds)".into(),
    };
    let title = format!("{base_title}{suffix}");
    let title = title.as_str();
    let caption = build_caption(summaries);
    let ext = output_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("html");

    match ext {
        "html" | "htm" => build::render_html(output_path, builds, title, caption.as_deref()),
        "gnuplot" | "gp" => {
            build::render_gnuplot(output_path, None, builds, title, caption.as_deref())
        }
        "svg" | "png" => {
            let gp_path = output_path.with_extension("gnuplot");
            build::render_gnuplot(
                &gp_path,
                Some(output_path),
                builds,
                title,
                caption.as_deref(),
            )?;
            run_gnuplot(&gp_path, output_path);
            Ok(())
        }
        _ => Err(format!("Unsupported format: {ext}. Use .html, .svg, .png, or .gnuplot").into()),
    }
}

pub fn export_csv(
    summaries: &[RunSummary],
    output_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut f = std::fs::File::create(output_path)?;
    writeln!(
        f,
        "index_name,index_type,data_type,search_params,k,recall,qps,avg_latency_us,p50_us,p95_us,p99_us,p999_us"
    )?;
    for summary in summaries {
        for r in &summary.search_results {
            writeln!(
                f,
                "{},{},{},{},{},{:.6},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1}",
                r.index_name,
                r.index_type,
                r.data_type.as_deref().unwrap_or(""),
                r.search_params,
                r.k,
                r.avg_recall,
                r.qps,
                r.avg_latency_us,
                r.p50_latency_us,
                r.p95_latency_us,
                r.p99_latency_us,
                r.p999_latency_us,
            )?;
        }
    }
    Ok(())
}

fn group_label(r: &SearchResult, per_config: bool) -> String {
    if per_config {
        return r.index_name.clone();
    }
    // Group by algo family — use index_type, with a readable name —
    // then split by data type so e.g. pgvector's bit/halfvec frontiers
    // are separate from the full-precision vector one.
    let family = match r.index_type.as_str() {
        "meerkat" => "Meerkat",
        "pgvector-hnsw" => "pgvector HNSW",
        other => other,
    };
    match &r.data_type {
        Some(dt) => format!("{family} ({dt})"),
        None => family.to_string(),
    }
}

fn prepare_plot_data(summaries: &[RunSummary], title: &str, opts: &PlotOptions) -> PlotData {
    let mut by_group: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    for summary in summaries {
        for r in &summary.search_results {
            let qps = if opts.use_wall_qps && r.qps_wall > 0.0 {
                r.qps_wall
            } else {
                r.qps
            };
            let label = group_label(r, opts.per_config);
            by_group.entry(label).or_default().push((r.avg_recall, qps));
        }
    }

    let mut series = Vec::new();

    for (name, mut points) in by_group {
        points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        if opts.min_recall > 0.0 {
            points.retain(|&(r, _)| r >= opts.min_recall);
        }
        if points.is_empty() {
            continue;
        }

        let pareto_points = compute_pareto_for_points(&points);
        let pareto_set: std::collections::HashSet<(u64, u64)> = pareto_points
            .iter()
            .map(|(r, q)| (r.to_bits(), q.to_bits()))
            .collect();

        // In per-algo mode (default), only show the Pareto frontier.
        // In per-config mode, also show non-Pareto points.
        let other_points = if opts.per_config {
            let mut others = Vec::new();
            for &(recall, qps) in &points {
                if !pareto_set.contains(&(recall.to_bits(), qps.to_bits())) {
                    others.push((recall, qps));
                }
            }
            others
        } else {
            Vec::new()
        };

        series.push(PlotSeries {
            name,
            pareto_points,
            other_points,
        });
    }

    PlotData {
        title: title.to_string(),
        caption: None,
        series,
    }
}

/// Build a one/two-line metadata caption for plots from the run's recorded
/// environment. The first summary carrying any metadata is treated as
/// representative (a report combines result files from a single run).
///
/// Line 1: dataset · run date · machine. The machine is the hardware
/// descriptor (CPU model, vCPU count, RAM) — always shown — prefixed with
/// the cloud instance type when one was captured.
/// Line 2: PG version · a few key settings · the extension versions tested.
///
/// Returns `None` when no summary has metadata (e.g. plotting bare result
/// files), so callers can render a title with no sub-line.
fn build_caption(summaries: &[RunSummary]) -> Option<String> {
    let meta = summaries
        .iter()
        .find(|s| s.environment.is_some() || s.pg_runtime.is_some() || !s.dataset.is_empty())?;

    let mut line1: Vec<String> = Vec::new();
    if !meta.dataset.is_empty() {
        line1.push(meta.dataset.clone());
    }
    if let Some(env) = &meta.environment {
        // Date only (drop the time/zone) keeps the caption compact.
        let date = env.timestamp.split('T').next().unwrap_or(&env.timestamp);
        line1.push(date.to_string());

        let h = &env.host;
        let hw = format!(
            "{}, {} vCPU, {:.0} GB",
            h.cpu_model.trim(),
            h.cpu_count,
            h.memory_gb
        );
        // Hardware is always shown; the instance type, when known, leads.
        let machine = match &h.instance_type {
            Some(it) => format!("{it}, {hw}"),
            None => hw,
        };
        line1.push(machine);
    }

    let mut line2: Vec<String> = Vec::new();
    if let Some(pg) = &meta.pg_runtime {
        // "PostgreSQL 18.4 (Ubuntu ...)" -> "PG 18.4"
        let ver = pg.version.split_whitespace().nth(1).unwrap_or(&pg.version);
        line2.push(format!("PG {ver}"));
        for key in [
            "shared_buffers",
            "work_mem",
            "max_parallel_maintenance_workers",
        ] {
            if let Some(v) = pg.settings.get(key) {
                line2.push(format!("{key}={v}"));
            }
        }
        // Extensions under test, compact: SQL version, plus a shortened git
        // commit when the extension reports one (Meerkat does). "vector" is
        // pgvector's extension name, so label it as such.
        let mut exts: Vec<String> = Vec::new();
        for (key, display) in [
            ("meerkat", "meerkat"),
            ("vchord", "vchord"),
            ("vector", "pgvector"),
        ] {
            if let Some(e) = pg.extensions.get(key) {
                let tag = match &e.git_commit {
                    Some(c) => {
                        let short: String = c.chars().take(7).collect();
                        format!("{display} {} ({short})", e.version)
                    }
                    None => format!("{display} {}", e.version),
                };
                exts.push(tag);
            }
        }
        if !exts.is_empty() {
            line2.push(exts.join(", "));
        }
    }

    let mut lines = Vec::new();
    // The run comment (--comment) leads: it is the human description of
    // why the run exists, so it is the first thing to read under the title.
    if let Some(comment) = meta
        .environment
        .as_ref()
        .and_then(|env| env.comment.as_deref())
    {
        lines.push(format!("\u{201c}{comment}\u{201d}"));
    }
    if !line1.is_empty() {
        lines.push(line1.join("  |  "));
    }
    if !line2.is_empty() {
        lines.push(line2.join("  |  "));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Compute Pareto frontier using the same greedy algorithm as ann-benchmarks:
/// sort by QPS descending, greedily include points that improve recall.
/// Then remove staircase artifacts by keeping only points where QPS
/// drops by at least 5% from the previous point, avoiding dense
/// clusters from different configs at similar recall values.
fn compute_pareto_for_points(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut sorted = points.to_vec();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let mut frontier = Vec::new();
    let mut best_recall = f64::NEG_INFINITY;

    for (recall, qps) in sorted {
        if recall > best_recall {
            best_recall = recall;
            frontier.push((recall, qps));
        }
    }

    frontier.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    frontier
}

/// Strip textLength/lengthAdjust from SVG to prevent gnuplot's
/// text squeezing. Gnuplot's SVG terminal emits fixed textLength
/// attributes that compress text horizontally in some viewers.
fn fix_svg_text(path: &Path) {
    if let Ok(svg) = std::fs::read_to_string(path) {
        let fixed = svg
            .replace(" textLength='", " _textLength='")
            .replace(" lengthAdjust='", " _lengthAdjust='");
        let _ = std::fs::write(path, fixed);
    }
}

pub(crate) fn logit(p: f64) -> f64 {
    (p.clamp(0.001, 0.999) / (1.0 - p.clamp(0.001, 0.999))).ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(index_type: &str, data_type: Option<&str>, name: &str) -> SearchResult {
        SearchResult {
            index_name: name.to_string(),
            index_type: index_type.to_string(),
            data_type: data_type.map(str::to_string),
            search_params: serde_json::json!({}),
            k: 10,
            num_queries: 1,
            num_runs: 1,
            avg_recall: 0.9,
            qps: 100.0,
            avg_latency_us: 0.0,
            p50_latency_us: 0.0,
            p95_latency_us: 0.0,
            p99_latency_us: 0.0,
            p999_latency_us: 0.0,
            wall_time_secs: 0.0,
            qps_wall: 0.0,
            query_results: Vec::new(),
        }
    }

    #[test]
    fn pareto_groups_pgvector_by_data_type() {
        // Full-precision vector keeps the bare family label; the quantized
        // encodings each get their own Pareto frontier.
        let full = result("pgvector-hnsw", None, "hnsw-m24");
        let bit_raw = result("pgvector-hnsw", Some("bit"), "hnsw-m24-bq-raw");
        let bit_rr = result("pgvector-hnsw", Some("bit"), "hnsw-m24-bq-rerank");
        let half = result("pgvector-hnsw", Some("halfvec"), "hnsw-halfvec");

        assert_eq!(group_label(&full, false), "pgvector HNSW");
        assert_eq!(group_label(&bit_raw, false), "pgvector HNSW (bit)");
        // raw and rerank on the same encoding share one frontier.
        assert_eq!(group_label(&bit_rr, false), "pgvector HNSW (bit)");
        assert_eq!(group_label(&half, false), "pgvector HNSW (halfvec)");
    }

    #[test]
    fn per_config_groups_by_series_name() {
        let bit_raw = result("pgvector-hnsw", Some("bit"), "hnsw-m24-bq-raw");
        let bit_rr = result("pgvector-hnsw", Some("bit"), "hnsw-m24-bq-rerank");
        // In per-config mode each named query is its own series, so raw and
        // rerank stay distinct even though they share a data type.
        assert_eq!(group_label(&bit_raw, true), "hnsw-m24-bq-raw");
        assert_eq!(group_label(&bit_rr, true), "hnsw-m24-bq-rerank");
    }

    fn summary_with_meta(instance_type: Option<&str>) -> RunSummary {
        use crate::metrics::{ExtensionInfo, HostInfo, PgRuntimeInfo, RekallInfo, RunEnvironment};
        use std::collections::BTreeMap;

        let mut settings = BTreeMap::new();
        settings.insert("shared_buffers".into(), "16 GB".into());
        settings.insert("work_mem".into(), "512 MB".into());
        let mut extensions = BTreeMap::new();
        extensions.insert(
            "meerkat".into(),
            ExtensionInfo {
                version: "0.1.0".into(),
                git_commit: Some("65df24d3ad9f".into()),
                library_version: None,
            },
        );
        extensions.insert(
            "vector".into(),
            ExtensionInfo {
                version: "0.8.2".into(),
                git_commit: None,
                library_version: None,
            },
        );

        RunSummary {
            dataset: "cohere-5m".into(),
            environment: Some(RunEnvironment {
                host: HostInfo {
                    hostname: "ip-10-0-0-1".into(),
                    os: "Ubuntu 24.04".into(),
                    kernel: "6.17".into(),
                    cpu_model: "Intel Xeon 6975P-C".into(),
                    cpu_count: 8,
                    memory_gb: 61.8,
                    ip_address: None,
                    instance_type: instance_type.map(str::to_string),
                },
                rekall: RekallInfo {
                    version: "0.1.0".into(),
                    git_commit: None,
                    git_branch: None,
                    note: String::new(),
                },
                timestamp: "2026-06-10T09:56:42+0000".into(),
                comment: None,
            }),
            config: serde_json::Value::Null,
            pg_runtime: Some(PgRuntimeInfo {
                version: "PostgreSQL 18.4 (Ubuntu 18.4-1) on x86_64".into(),
                settings,
                extensions,
            }),
            index_builds: Vec::new(),
            search_results: Vec::new(),
            pareto: Vec::new(),
        }
    }

    #[test]
    fn caption_includes_hardware_and_instance_type() {
        let cap = build_caption(&[summary_with_meta(Some("r8i.2xlarge"))]).unwrap();
        // Instance type leads, hardware always follows it.
        assert!(cap.contains("r8i.2xlarge, Intel Xeon 6975P-C, 8 vCPU, 62 GB"));
        // Dataset, date, PG version, settings, extension versions present.
        assert!(cap.contains("cohere-5m"));
        assert!(cap.contains("2026-06-10"));
        assert!(cap.contains("PG 18.4"));
        assert!(cap.contains("shared_buffers=16 GB"));
        assert!(cap.contains("meerkat 0.1.0 (65df24d)"));
        assert!(cap.contains("pgvector 0.8.2"));
    }

    #[test]
    fn caption_leads_with_run_comment() {
        let mut summary = summary_with_meta(Some("r8i.2xlarge"));
        summary.environment.as_mut().unwrap().comment = Some("testing new feature X".into());
        let cap = build_caption(&[summary]).unwrap();
        // The comment is the first caption line, quoted.
        assert!(cap.starts_with("\u{201c}testing new feature X\u{201d}\n"));
        // The usual metadata lines still follow.
        assert!(cap.contains("cohere-5m"));
    }

    #[test]
    fn caption_falls_back_to_hardware_without_instance_type() {
        let cap = build_caption(&[summary_with_meta(None)]).unwrap();
        // Hardware descriptor still shown; no stray instance-type prefix.
        assert!(cap.contains("Intel Xeon 6975P-C, 8 vCPU, 62 GB"));
        assert!(!cap.contains("r8i"));
    }

    #[test]
    fn caption_is_none_without_metadata() {
        let bare = RunSummary {
            dataset: String::new(),
            environment: None,
            config: serde_json::Value::Null,
            pg_runtime: None,
            index_builds: Vec::new(),
            search_results: Vec::new(),
            pareto: Vec::new(),
        };
        assert!(build_caption(&[bare]).is_none());
    }
}
