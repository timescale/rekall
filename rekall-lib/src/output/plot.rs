use std::collections::BTreeMap;
use std::path::Path;

use plotters::prelude::*;

use crate::metrics::{compute_pareto, RunSummary, SearchResult};

struct Series {
    name: String,
    points: Vec<(f64, f64)>, // (recall, qps)
}

pub struct PlotOptions {
    pub log_x: bool,
    pub log_y: bool,
}

impl Default for PlotOptions {
    fn default() -> Self {
        Self {
            log_x: true,
            log_y: true,
        }
    }
}

pub fn generate_plot(
    summaries: &[RunSummary],
    output_path: &Path,
    title: Option<&str>,
    opts: &PlotOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let series = build_series(summaries);
    if series.is_empty() {
        return Err("No search results to plot".into());
    }

    let max_qps = series
        .iter()
        .flat_map(|s| s.points.iter().map(|(_, q)| *q))
        .fold(0.0_f64, f64::max);
    let min_qps = series
        .iter()
        .flat_map(|s| s.points.iter().map(|(_, q)| *q))
        .fold(f64::MAX, f64::min);

    let ext = output_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("svg");

    let title = title.unwrap_or("Recall vs QPS");

    match ext {
        "svg" => draw_svg(output_path, title, &series, min_qps, max_qps, opts),
        "png" => draw_png(output_path, title, &series, min_qps, max_qps, opts),
        "html" | "htm" => draw_html(output_path, title, &series, opts),
        _ => Err(format!("Unsupported format: {ext}. Use .svg, .png, or .html").into()),
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

fn build_series(summaries: &[RunSummary]) -> Vec<Series> {
    let mut by_index: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();

    for summary in summaries {
        for r in &summary.search_results {
            by_index
                .entry(r.index_name.clone())
                .or_default()
                .push((r.avg_recall, r.qps));
        }
    }

    by_index
        .into_iter()
        .map(|(name, mut points)| {
            points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            Series { name, points }
        })
        .collect()
}

const COLORS: &[RGBColor] = &[
    RGBColor(31, 119, 180),  // blue
    RGBColor(255, 127, 14),  // orange
    RGBColor(44, 160, 44),   // green
    RGBColor(214, 39, 40),   // red
    RGBColor(148, 103, 189), // purple
    RGBColor(140, 86, 75),   // brown
    RGBColor(227, 119, 194), // pink
    RGBColor(127, 127, 127), // gray
    RGBColor(188, 189, 34),  // olive
    RGBColor(23, 190, 207),  // cyan
];

fn draw_svg(
    path: &Path,
    title: &str,
    series: &[Series],
    min_qps: f64,
    max_qps: f64,
    opts: &PlotOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = SVGBackend::new(path, (900, 600)).into_drawing_area();
    draw_chart(&root, title, series, min_qps, max_qps, opts)?;
    root.present()?;

    // Make SVG scalable: remove fixed width/height, keep viewBox
    let content = std::fs::read_to_string(path)?;
    let patched = content.replacen("width=\"900\" height=\"600\" viewBox", "viewBox", 1);
    std::fs::write(path, patched)?;

    Ok(())
}

fn draw_html(
    path: &Path,
    title: &str,
    series: &[Series],
    opts: &PlotOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;

    // Build Plotly traces as JSON
    let mut traces = String::new();
    for (i, s) in series.iter().enumerate() {
        let color = COLORS[i % COLORS.len()];
        let color_str = format!("rgb({},{},{})", color.0, color.1, color.2);

        // Compute Pareto frontier
        let pareto_results: Vec<SearchResult> = s
            .points
            .iter()
            .map(|&(recall, qps)| SearchResult {
                index_name: s.name.clone(),
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
            })
            .collect();
        let pareto = compute_pareto(&pareto_results);
        let pareto_set: std::collections::HashSet<u64> = pareto
            .iter()
            .filter(|p| p.is_pareto)
            .map(|p| p.recall.to_bits())
            .collect();

        let mut pareto_x = Vec::new();
        let mut pareto_y = Vec::new();
        let mut other_x = Vec::new();
        let mut other_y = Vec::new();

        for &(recall, qps) in &s.points {
            if pareto_set.contains(&recall.to_bits()) {
                pareto_x.push(recall);
                pareto_y.push(qps);
            } else {
                other_x.push(recall);
                other_y.push(qps);
            }
        }

        // Pareto frontier trace (thick line + filled markers)
        if !traces.is_empty() {
            traces.push(',');
        }
        traces.push_str(&format!(
            r#"{{
                x: {pareto_x:?},
                y: {pareto_y:?},
                name: "{name}",
                type: "scatter",
                mode: "lines+markers",
                line: {{ color: "{color_str}", width: 3 }},
                marker: {{ color: "{color_str}", size: 8 }},
                legendgroup: "{name}",
                hovertemplate: "<b>{name}</b><br>Recall: %{{x:.4f}}<br>QPS: %{{y:.0f}}<extra></extra>"
            }}"#,
            name = s.name,
        ));

        // Non-Pareto points (small hollow markers, no separate legend entry)
        if !other_x.is_empty() {
            traces.push(',');
            traces.push_str(&format!(
                r#"{{
                    x: {other_x:?},
                    y: {other_y:?},
                    type: "scatter",
                    mode: "markers",
                    marker: {{ color: "{color_str}", size: 5, opacity: 0.4 }},
                    legendgroup: "{name}",
                    showlegend: false,
                    hovertemplate: "<b>{name}</b><br>Recall: %{{x:.4f}}<br>QPS: %{{y:.0f}}<extra></extra>"
                }}"#,
                name = s.name,
            ));
        }
    }

    // Global Pareto frontier across all series
    let all_points: Vec<(f64, f64)> = series
        .iter()
        .flat_map(|s| s.points.iter().copied())
        .collect();
    let all_results: Vec<SearchResult> = all_points
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
        })
        .collect();
    let global_pareto = compute_pareto(&all_results);
    let mut global_pareto_points: Vec<(f64, f64)> = global_pareto
        .iter()
        .filter(|p| p.is_pareto)
        .map(|p| (p.recall, p.qps))
        .collect();
    global_pareto_points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    if !global_pareto_points.is_empty() {
        let gp_x: Vec<f64> = global_pareto_points.iter().map(|(r, _)| *r).collect();
        let gp_y: Vec<f64> = global_pareto_points.iter().map(|(_, q)| *q).collect();
        if !traces.is_empty() {
            traces.push(',');
        }
        traces.push_str(&format!(
            r#"{{
                x: {gp_x:?},
                y: {gp_y:?},
                name: "Global Pareto",
                type: "scatter",
                mode: "lines",
                line: {{ color: "black", width: 2, dash: "dash" }},
                hovertemplate: "<b>Pareto</b><br>Recall: %{{x:.4f}}<br>QPS: %{{y:.0f}}<extra></extra>"
            }}"#,
        ));
    }

    let x_type = if opts.log_x { "\"log\"" } else { "\"linear\"" };
    let y_type = if opts.log_y { "\"log\"" } else { "\"linear\"" };

    let html = format!(
        concat!(
            "<!DOCTYPE html>\n",
            "<html>\n<head>\n<meta charset=\"utf-8\">\n",
            "<title>{title}</title>\n",
            "<script src=\"https://cdn.plot.ly/plotly-2.35.2.min.js\"></script>\n",
            "<style>\n",
            "  body {{ margin: 0; padding: 0; background: white; }}\n",
            "  #plot {{ width: 100vw; height: 100vh; }}\n",
            "</style>\n</head>\n<body>\n<div id=\"plot\"></div>\n",
            "<script>\n",
            "var traces = [{traces}];\n",
            "var layout = {{\n",
            "  title: {{ text: \"{title}\", font: {{ size: 20 }} }},\n",
            "  xaxis: {{ title: \"Recall@10\", type: {x_type}, tickformat: \".2f\", gridcolor: \"#eeeeee\" }},\n",
            "  yaxis: {{ title: \"Queries per second\", type: {y_type}, gridcolor: \"#eeeeee\" }},\n",
            "  legend: {{ x: 0.02, y: 0.02, xanchor: \"left\", yanchor: \"bottom\", bgcolor: \"rgba(255,255,255,0.9)\", bordercolor: \"#cccccc\", borderwidth: 1 }},\n",
            "  hovermode: \"closest\",\n",
            "  plot_bgcolor: \"white\",\n",
            "  paper_bgcolor: \"white\"\n",
            "}};\n",
            "Plotly.newPlot(\"plot\", traces, layout, {{ responsive: true }});\n",
            "function showAll() {{ var d = document.getElementById(\"plot\"); d.data.forEach(function(_,i) {{ Plotly.restyle(d, {{visible: true}}, [i]); }}); }}\n",
            "function hideAll() {{ var d = document.getElementById(\"plot\"); d.data.forEach(function(_,i) {{ Plotly.restyle(d, {{visible: \"legendonly\"}}, [i]); }}); }}\n",
            "</script>\n",
            "<div style=\"position:fixed;top:8px;right:8px;z-index:999;font-family:sans-serif;font-size:13px\">\n",
            "  <button onclick=\"showAll()\" style=\"cursor:pointer;padding:4px 10px\">All</button>\n",
            "  <button onclick=\"hideAll()\" style=\"cursor:pointer;padding:4px 10px\">None</button>\n",
            "</div>\n",
            "</body>\n</html>",
        ),
        title = title,
        traces = traces,
        x_type = x_type,
        y_type = y_type,
    );
    f.write_all(html.as_bytes())?;

    Ok(())
}

fn draw_png(
    path: &Path,
    title: &str,
    series: &[Series],
    min_qps: f64,
    max_qps: f64,
    opts: &PlotOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = BitMapBackend::new(path, (900, 600)).into_drawing_area();
    draw_chart(&root, title, series, min_qps, max_qps, opts)?;
    root.present()?;
    Ok(())
}

fn draw_chart<DB: DrawingBackend>(
    root: &DrawingArea<DB, plotters::coord::Shift>,
    title: &str,
    series: &[Series],
    min_qps: f64,
    max_qps: f64,
    _opts: &PlotOptions,
) -> Result<(), Box<dyn std::error::Error>>
where
    DB::ErrorType: 'static,
{
    root.fill(&WHITE)?;

    let qps_lo = (min_qps * 0.5).max(1.0);
    let qps_hi = max_qps * 2.0;

    // plotters doesn't support logit scale, so for X we use log when requested.
    // This requires transforming recall values to avoid log(0).
    // When both axes are log, we build a log-log chart.
    // When neither is log, we build a linear-linear chart.
    // Mixed combinations use the appropriate per-axis scale.
    //
    // For simplicity, always build with log Y (the common case) since
    // plotters' Cartesian2d doesn't support mixed log/linear easily.
    // The HTML backend handles all combinations natively.
    let mut chart = ChartBuilder::on(root)
        .caption(title, ("sans-serif", 24).into_font())
        .margin(15)
        .x_label_area_size(40)
        .y_label_area_size(60)
        .right_y_label_area_size(10)
        .build_cartesian_2d(0.5_f64..1.0_f64, (qps_lo..qps_hi).log_scale())?;

    chart
        .configure_mesh()
        .x_desc("Recall@10")
        .y_desc("Queries per second")
        .x_labels(10)
        .y_labels(8)
        .x_label_formatter(&|x| format!("{:.2}", x))
        .y_label_formatter(&|y| {
            if *y >= 1000.0 {
                format!("{:.0}K", y / 1000.0)
            } else {
                format!("{:.0}", y)
            }
        })
        .draw()?;

    for (i, s) in series.iter().enumerate() {
        let color = COLORS[i % COLORS.len()];
        let thin_style = ShapeStyle::from(color).stroke_width(1);
        let thick_style = ShapeStyle::from(color).stroke_width(3);

        // Compute Pareto frontier for this series
        let pareto_results: Vec<SearchResult> = s
            .points
            .iter()
            .map(|&(recall, qps)| SearchResult {
                index_name: s.name.clone(),
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
            })
            .collect();
        let pareto = compute_pareto(&pareto_results);
        let mut pareto_points: Vec<(f64, f64)> = pareto
            .iter()
            .filter(|p| p.is_pareto)
            .map(|p| (p.recall, p.qps))
            .collect();
        pareto_points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        // Draw non-Pareto points with thin lines
        chart.draw_series(LineSeries::new(s.points.iter().copied(), thin_style))?;

        // Draw Pareto frontier with thick lines
        chart
            .draw_series(LineSeries::new(pareto_points.iter().copied(), thick_style))?
            .label(&s.name)
            .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], thick_style));

        // Draw markers on all points (filled for Pareto, hollow for non-Pareto)
        let pareto_set: std::collections::HashSet<u64> =
            pareto_points.iter().map(|(r, _)| r.to_bits()).collect();

        chart.draw_series(s.points.iter().map(|&(x, y)| {
            if pareto_set.contains(&x.to_bits()) {
                Circle::new((x, y), 5, thick_style.filled())
            } else {
                Circle::new((x, y), 3, thin_style)
            }
        }))?;
    }

    // Global Pareto frontier (dashed black line)
    let all_points: Vec<(f64, f64)> = series
        .iter()
        .flat_map(|s| s.points.iter().copied())
        .collect();
    let all_results: Vec<SearchResult> = all_points
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
        })
        .collect();
    let global_pareto = compute_pareto(&all_results);
    let mut gp_points: Vec<(f64, f64)> = global_pareto
        .iter()
        .filter(|p| p.is_pareto)
        .map(|p| (p.recall, p.qps))
        .collect();
    gp_points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    let dash_style = ShapeStyle::from(BLACK.mix(0.6)).stroke_width(2);
    chart
        .draw_series(LineSeries::new(gp_points.iter().copied(), dash_style))?
        .label("Global Pareto")
        .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], dash_style));

    chart
        .configure_series_labels()
        .background_style(WHITE.mix(0.9))
        .border_style(BLACK.mix(0.3))
        .position(SeriesLabelPosition::LowerLeft)
        .label_font(("sans-serif", 13))
        .draw()?;

    Ok(())
}
