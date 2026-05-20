use std::collections::BTreeMap;
use std::path::Path;

use crate::metrics::{compute_pareto, RunSummary, SearchResult};

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
    pub series: Vec<PlotSeries>,
    pub global_pareto: Vec<(f64, f64)>,
}

pub struct PlotSeries {
    pub name: String,
    pub pareto_points: Vec<(f64, f64)>,
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
    let data = prepare_plot_data(summaries, title.unwrap_or("Recall vs QPS"), opts);
    if data.series.is_empty() {
        return Err("No search results to plot".into());
    }

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
            match std::process::Command::new("gnuplot").arg(&gp_path).status() {
                Ok(s) if s.success() => {}
                _ => {
                    eprintln!(
                        "gnuplot not found or failed. Run manually:\n  gnuplot {}",
                        gp_path.display()
                    );
                }
            }
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
        "index_name,index_type,search_params,k,recall,qps,avg_latency_us,p50_us,p95_us,p99_us,p999_us"
    )?;
    for summary in summaries {
        for r in &summary.search_results {
            writeln!(
                f,
                "{},{},{},{},{:.6},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1}",
                r.index_name,
                r.index_type,
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
        r.index_name.clone()
    } else {
        // Group by algo family — use index_type, with a readable name
        match r.index_type.as_str() {
            "meerkat" => "Meerkat".to_string(),
            "pgvector-hnsw" => "pgvector HNSW".to_string(),
            other => other.to_string(),
        }
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

    let mut all_points = Vec::new();
    let mut series = Vec::new();

    for (name, mut points) in by_group {
        points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        if opts.min_recall > 0.0 {
            points.retain(|&(r, _)| r >= opts.min_recall);
        }
        if points.is_empty() {
            continue;
        }

        all_points.extend(points.iter().copied());

        let pareto = compute_pareto_for_points(&points);
        let pareto_set: std::collections::HashSet<u64> =
            pareto.iter().map(|(r, _)| r.to_bits()).collect();

        let mut pareto_points = Vec::new();
        let mut other_points = Vec::new();
        for &(recall, qps) in &points {
            if pareto_set.contains(&recall.to_bits()) {
                pareto_points.push((recall, qps));
            } else {
                other_points.push((recall, qps));
            }
        }

        series.push(PlotSeries {
            name,
            pareto_points,
            other_points,
        });
    }

    let global_pareto = compute_pareto_for_points(&all_points);

    PlotData {
        title: title.to_string(),
        series,
        global_pareto,
    }
}

fn compute_pareto_for_points(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let results: Vec<SearchResult> = points
        .iter()
        .map(|&(recall, qps)| SearchResult {
            index_name: String::new(),
            index_type: String::new(),
            search_params: serde_json::json!({}),
            k: 0,
            num_queries: 0,
            num_runs: 0,
            avg_recall: recall,
            qps,
            avg_latency_us: 0.0,
            p50_latency_us: 0.0,
            p95_latency_us: 0.0,
            p99_latency_us: 0.0,
            p999_latency_us: 0.0,
            wall_time_secs: 0.0,
            qps_wall: qps,
        })
        .collect();
    let pareto = compute_pareto(&results);
    let mut pts: Vec<(f64, f64)> = pareto
        .iter()
        .filter(|p| p.is_pareto)
        .map(|p| (p.recall, p.qps))
        .collect();
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    pts
}

pub(crate) fn logit(p: f64) -> f64 {
    (p.clamp(0.001, 0.999) / (1.0 - p.clamp(0.001, 0.999))).ln()
}
