//! Index build-time bar chart (one bar per index, tallest first). HTML uses
//! Plotly; SVG/PNG go through gnuplot. Bars are build time in seconds; each
//! bar is labelled with the index size and the worker count shows in the
//! hover / label.

use std::io::Write;
use std::path::Path;

use crate::metrics::IndexBuildResult;

use super::gnuplot::title_string;

type R = Result<(), Box<dyn std::error::Error>>;

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Builds sorted by descending build time (tallest bar first).
fn sorted(builds: &[IndexBuildResult]) -> Vec<&IndexBuildResult> {
    let mut v: Vec<&IndexBuildResult> = builds.iter().collect();
    v.sort_by(|a, b| {
        b.build_time
            .as_secs_f64()
            .partial_cmp(&a.build_time.as_secs_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v
}

fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn render_html(
    path: &Path,
    builds: &[IndexBuildResult],
    title: &str,
    caption: Option<&str>,
) -> R {
    let bars = sorted(builds);
    let names: Vec<String> = bars.iter().map(|b| b.index_name.clone()).collect();
    let secs: Vec<f64> = bars.iter().map(|b| b.build_time.as_secs_f64()).collect();
    let labels: Vec<String> = bars
        .iter()
        .map(|b| format!("{:.0} MB", mb(b.index_size_bytes)))
        .collect();
    let hover: Vec<String> = bars
        .iter()
        .map(|b| {
            format!(
                "{:.1}s · {:.0} MB · {} workers",
                b.build_time.as_secs_f64(),
                mb(b.index_size_bytes),
                b.parallel_workers
            )
        })
        .collect();

    let caption_html = match caption {
        Some(c) => format!(
            "<div class=\"meta\">{}</div>",
            c.lines().map(esc_html).collect::<Vec<_>>().join("<br>")
        ),
        None => String::new(),
    };

    let html = format!(
        concat!(
            "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>{title}</title>\n",
            "<script src=\"https://cdn.plot.ly/plotly-2.35.2.min.js\"></script>\n<style>\n",
            "  html,body{{height:100%;margin:0;background:white;font-family:system-ui,sans-serif}}\n",
            "  body{{display:flex;flex-direction:column}}\n",
            "  #header{{flex:0 0 auto;padding:10px 16px 6px;text-align:center}}\n",
            "  #header .title{{font-size:20px;font-weight:600;color:#222}}\n",
            "  #header .meta{{margin-top:4px;font-size:12px;color:#555}}\n",
            "  #plot{{flex:1 1 auto;min-height:0;width:100%}}\n",
            "</style></head><body>\n",
            "<div id=\"header\"><div class=\"title\">{title_h}</div>{caption_html}</div>\n",
            "<div id=\"plot\"></div>\n<script>\n",
            "var trace = {{ type:\"bar\", x:{names:?}, y:{secs:?}, text:{labels:?},\n",
            "  textposition:\"outside\", cliponaxis:false, hovertext:{hover:?},\n",
            "  hovertemplate:\"<b>%{{x}}</b><br>%{{hovertext}}<extra></extra>\",\n",
            "  marker:{{color:\"rgb(31,119,180)\"}} }};\n",
            "var layout = {{ margin:{{t:20}},\n",
            "  yaxis:{{ title:\"Build time (seconds)\", gridcolor:\"#eeeeee\", rangemode:\"tozero\" }},\n",
            "  xaxis:{{ automargin:true, tickangle:-40 }},\n",
            "  plot_bgcolor:\"white\", paper_bgcolor:\"white\" }};\n",
            "Plotly.newPlot(\"plot\",[trace],layout,{{responsive:true}});\n",
            "</script></body></html>",
        ),
        title = esc_html(title),
        title_h = esc_html(title),
        caption_html = caption_html,
        names = names,
        secs = secs,
        labels = labels,
        hover = hover,
    );
    std::fs::File::create(path)?.write_all(html.as_bytes())?;
    Ok(())
}

pub fn render_gnuplot(
    gp_path: &Path,
    output_path: Option<&Path>,
    builds: &[IndexBuildResult],
    title: &str,
    caption: Option<&str>,
) -> R {
    let bars = sorted(builds);
    let mut f = std::fs::File::create(gp_path)?;

    if let Some(out) = output_path {
        let ext = out.extension().and_then(|e| e.to_str()).unwrap_or("svg");
        let term = if ext == "png" {
            "pngcairo size 1200,800 enhanced font 'sans,12' background '#ffffff'"
        } else {
            "svg size 1200,800 dynamic enhanced font 'sans,12' background '#ffffff'"
        };
        writeln!(f, "set terminal {term}")?;
        writeln!(f, "set output '{}'", out.display())?;
    } else {
        writeln!(f, "set terminal qt size 1200,800 enhanced font 'sans,12'")?;
    }

    writeln!(f)?;
    writeln!(f, "set title \"{}\"", title_string(title, caption))?;
    writeln!(f, "set ylabel 'Build time (seconds)'")?;
    writeln!(f, "set grid ytics")?;
    writeln!(f, "set yrange [0:*]")?;
    // A little x padding off the frame and ~8% headroom so the tallest bar's
    // size label isn't clipped at the top.
    writeln!(f, "set offsets graph 0.02, graph 0.02, graph 0.08, 0")?;
    writeln!(f, "set style fill solid 0.65 border -1")?;
    writeln!(f, "set boxwidth 0.6 relative")?;
    // Index names are long and share a prefix; vertical labels avoid the
    // horizontal overlap that angled labels produce, and a generous bottom
    // margin keeps them on-canvas.
    writeln!(f, "set xtics noenhanced rotate by -90 font ',8'")?;
    writeln!(f, "set bmargin 16")?;
    writeln!(f, "unset key")?;
    writeln!(f)?;

    writeln!(f, "$builds << EOD")?;
    for b in &bars {
        // "name" seconds size_mb  — name quoted so xtic(1) keeps it whole.
        writeln!(
            f,
            "\"{}\" {} {:.1}",
            b.index_name.replace('"', "'"),
            b.build_time.as_secs_f64(),
            mb(b.index_size_bytes)
        )?;
    }
    writeln!(f, "EOD")?;
    writeln!(f)?;

    writeln!(f, "plot \\")?;
    writeln!(
        f,
        "  $builds using 2:xtic(1) with boxes lc rgb '#1f77b4' notitle, \\"
    )?;
    writeln!(
        f,
        "  $builds using 0:2:(sprintf('%.0f MB', $3)) with labels offset 0,0.8 font ',8' notitle"
    )?;

    if output_path.is_none() {
        writeln!(f, "pause -1 'Press Enter to close'")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn build(name: &str, secs: u64, size: u64, workers: u32) -> IndexBuildResult {
        IndexBuildResult {
            index_name: name.to_string(),
            index_type: "meerkat".to_string(),
            params: serde_json::json!({}),
            build_time: Duration::from_secs(secs),
            index_size_bytes: size,
            parallel_workers: workers,
            monitor: None,
        }
    }

    #[test]
    fn sorted_is_descending_by_build_time() {
        let bs = vec![
            build("a", 100, 0, 1),
            build("b", 300, 0, 1),
            build("c", 200, 0, 1),
        ];
        let s = sorted(&bs);
        assert_eq!(s[0].index_name, "b");
        assert_eq!(s[1].index_name, "c");
        assert_eq!(s[2].index_name, "a");
    }

    #[test]
    fn html_has_bars_and_labels() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.html");
        render_html(
            &p,
            &[build("mkt-n16000", 600, 2_300_000_000, 8)],
            "Index build time",
            Some("cohere-5m | r8i"),
        )
        .unwrap();
        let html = std::fs::read_to_string(&p).unwrap();
        assert!(html.contains("type:\"bar\""));
        assert!(html.contains("mkt-n16000"));
        assert!(html.contains("Build time (seconds)"));
        assert!(html.contains("cohere-5m | r8i")); // caption
    }
}
