use std::io::Write;
use std::path::Path;

use super::{logit, PlotData, PlotOptions, PlotPoint, YMetric, COLORS};

pub fn render(
    path: &Path,
    data: &PlotData,
    opts: &PlotOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut f = std::fs::File::create(path)?;

    let xform_x = if opts.log_x {
        logit as fn(f64) -> f64
    } else {
        (|x| x) as fn(f64) -> f64
    };

    // The y hover line for the QPS metric reads the axis value; the
    // disk metric's exact value is carried in each point's hover text
    // (the axis value may be floored for the log scale).
    let y_line = match opts.y_metric {
        YMetric::Qps => "<br>QPS: %{y:.0f}",
        YMetric::DiskReadPerQuery => "",
    };

    let mut traces = String::new();

    for (i, s) in data.series.iter().enumerate() {
        let (r, g, b) = COLORS[i % COLORS.len()];
        let color_str = format!("rgb({r},{g},{b})");

        let coords = |points: &[PlotPoint]| {
            let x: Vec<f64> = points.iter().map(|p| xform_x(p.recall)).collect();
            let y: Vec<f64> = points.iter().map(|p| p.y).collect();
            let recall: Vec<f64> = points.iter().map(|p| p.recall).collect();
            let hover: Vec<&str> = points.iter().map(|p| p.hover.as_str()).collect();
            // serde_json (not {:?}) for the hover strings: Rust's debug
            // escaping is not valid JSON for all inputs.
            (x, y, recall, serde_json::to_string(&hover).unwrap())
        };

        let (pareto_x, pareto_y, pareto_recall, pareto_hover) = coords(&s.pareto_points);

        if !traces.is_empty() {
            traces.push(',');
        }
        traces.push_str(&format!(
            r#"{{
                x: {pareto_x:?},
                y: {pareto_y:?},
                customdata: {pareto_recall:?},
                text: {pareto_hover},
                name: "{name}",
                type: "scatter",
                mode: "lines+markers",
                line: {{ color: "{color_str}", width: 3 }},
                marker: {{ color: "{color_str}", size: 8 }},
                legendgroup: "{name}",
                hovertemplate: "<b>{name}</b><br>Recall: %{{customdata:.4f}}{y_line}%{{text}}<extra></extra>"
            }}"#,
            name = s.name,
        ));

        if !s.other_points.is_empty() && !opts.pareto_only {
            let (other_x, other_y, other_recall, other_hover) = coords(&s.other_points);

            traces.push(',');
            traces.push_str(&format!(
                r#"{{
                    x: {other_x:?},
                    y: {other_y:?},
                    customdata: {other_recall:?},
                    text: {other_hover},
                    type: "scatter",
                    mode: "markers",
                    marker: {{ color: "{color_str}", size: 5, opacity: 0.4 }},
                    legendgroup: "{name}",
                    showlegend: false,
                    hovertemplate: "<b>{name}</b><br>Recall: %{{customdata:.4f}}{y_line}%{{text}}<extra></extra>"
                }}"#,
                name = s.name,
            ));
        }
    }

    let y_type = if opts.log_y { "\"log\"" } else { "\"linear\"" };
    let min_r = if opts.min_recall > 0.0 {
        opts.min_recall
    } else {
        0.5
    };

    let x_axis_config = if opts.log_x {
        let tick_vals: Vec<f64> = [
            0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.925, 0.95, 0.975, 0.99, 0.995, 0.999,
        ]
        .iter()
        .filter(|&&v| v >= min_r)
        .map(|&v| logit(v))
        .collect();
        let tick_text: Vec<String> = [
            0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.925, 0.95, 0.975, 0.99, 0.995, 0.999,
        ]
        .iter()
        .filter(|&&v| v >= min_r)
        .map(|v| format!("{v}"))
        .collect();
        format!(
            "tickmode: \"array\", tickvals: {:?}, ticktext: {:?}, range: [{}, {}], gridcolor: \"#eeeeee\"",
            tick_vals, tick_text, logit(min_r), logit(0.999),
        )
    } else {
        format!("range: [{min_r}, 1.0], tickformat: \".2f\", gridcolor: \"#eeeeee\"")
    };

    // The title + metadata caption live in a plain HTML header above the
    // chart rather than in Plotly's in-chart title (which gets clipped when
    // the plot div fills the viewport). The body is a flex column: a
    // fixed-height header, then the plot flexes to fill the rest.
    let esc_html = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let title_html = esc_html(&data.title);
    let caption_html = match &data.caption {
        Some(caption) => {
            let lines = caption
                .lines()
                .map(esc_html)
                .collect::<Vec<_>>()
                .join("<br>");
            format!("<div class=\"meta\">{lines}</div>")
        }
        None => String::new(),
    };

    let html = format!(
        concat!(
            "<!DOCTYPE html>\n",
            "<html>\n<head>\n<meta charset=\"utf-8\">\n",
            "<title>{title}</title>\n",
            "<script src=\"https://cdn.plot.ly/plotly-2.35.2.min.js\"></script>\n",
            "<style>\n",
            "  html, body {{ height: 100%; margin: 0; padding: 0; background: white; }}\n",
            "  body {{ display: flex; flex-direction: column; font-family: sans-serif; }}\n",
            "  #header {{ flex: 0 0 auto; padding: 10px 16px 6px; text-align: center; }}\n",
            "  #header .title {{ font-size: 20px; font-weight: 600; color: #222; }}\n",
            "  #header .meta {{ margin-top: 4px; font-size: 12px; color: #555; }}\n",
            "  #plot {{ flex: 1 1 auto; min-height: 0; width: 100%; }}\n",
            "</style>\n</head>\n<body>\n",
            "<div id=\"header\"><div class=\"title\">{title_html}</div>{caption_html}</div>\n",
            "<div id=\"plot\"></div>\n",
            "<script>\n",
            "var traces = [{traces}];\n",
            "var layout = {{\n",
            "  margin: {{ t: 20 }},\n",
            "  xaxis: {{ title: \"Recall@10\", {x_axis_config} }},\n",
            "  yaxis: {{ title: \"{y_title}\", type: {y_type}, gridcolor: \"#eeeeee\" }},\n",
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
        title = title_html,
        title_html = title_html,
        caption_html = caption_html,
        traces = traces,
        x_axis_config = x_axis_config,
        y_title = opts.y_metric.axis_title(),
        y_type = y_type,
    );
    f.write_all(html.as_bytes())?;

    Ok(())
}
