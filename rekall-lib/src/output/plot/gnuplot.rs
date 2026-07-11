use std::io::Write;
use std::path::Path;

use super::{PlotData, PlotOptions, COLORS};

pub fn render(
    gp_path: &Path,
    output_path: Option<&Path>,
    data: &PlotData,
    opts: &PlotOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut f = std::fs::File::create(gp_path)?;

    // Compute data range for x-axis
    let mut data_min = 1.0_f64;
    let mut data_max = 0.0_f64;
    for s in &data.series {
        for p in s.pareto_points.iter().chain(&s.other_points) {
            if p.recall < data_min {
                data_min = p.recall;
            }
            if p.recall > data_max {
                data_max = p.recall;
            }
        }
    }
    if opts.min_recall > 0.0 {
        data_min = opts.min_recall;
    }

    // Terminal
    if let Some(out) = output_path {
        let ext = out.extension().and_then(|e| e.to_str()).unwrap_or("svg");
        match ext {
            "svg" => writeln!(
                f,
                "set terminal svg size 1200,800 dynamic enhanced font 'sans,12' background '#ffffff'"
            )?,
            "png" => writeln!(
                f,
                "set terminal pngcairo size 1200,800 enhanced font 'sans,12' background '#ffffff'"
            )?,
            _ => writeln!(
                f,
                "set terminal svg size 1200,800 dynamic enhanced font 'sans,12' background '#ffffff'"
            )?,
        }
        writeln!(f, "set output '{}'", out.display())?;
    } else {
        writeln!(f, "set terminal qt size 1200,800 enhanced font 'sans,12'")?;
    }

    writeln!(f)?;
    // Title, with the metadata caption appended as smaller extra lines.
    writeln!(
        f,
        "set title \"{}\"",
        title_string(&data.title, data.caption.as_deref())
    )?;
    writeln!(f, "set xlabel noenhanced 'Recall@10' offset 0,-1")?;
    writeln!(f, "set ylabel '{}'", opts.y_metric.axis_title())?;
    writeln!(f, "set grid")?;
    writeln!(f, "set key left bottom box opaque")?;
    writeln!(f, "set bmargin 7")?;
    writeln!(f, "set xtics rotate by -45")?;
    writeln!(f)?;

    // Y-axis
    if opts.log_y {
        writeln!(f, "set logscale y")?;
        let y_fmt = match opts.y_metric {
            super::YMetric::Qps => "%.0s%cQPS",
            super::YMetric::DiskReadPerQuery => "%.0s%cB",
        };
        writeln!(f, "set format y '{y_fmt}'")?;
    }

    // X-axis: logit scale via gnuplot's nonlinear axis
    if opts.log_x {
        let all_ticks: &[(f64, &str)] = &[
            (0.5, "0.5"),
            (0.6, "0.6"),
            (0.7, "0.7"),
            (0.8, "0.8"),
            (0.85, "0.85"),
            (0.9, "0.9"),
            (0.925, "0.925"),
            (0.95, "0.95"),
            (0.975, "0.975"),
            (0.99, "0.99"),
            (0.995, "0.995"),
            (0.999, "0.999"),
            (0.9999, "0.9999"),
        ];

        /* Pick range: snap to tick below data_min and above data_max */
        let mut x_min = data_min;
        for &(v, _) in all_ticks.iter().rev() {
            if v <= data_min {
                x_min = v;
                break;
            }
        }
        let mut x_max = data_max;
        for &(v, _) in all_ticks {
            if v >= data_max {
                x_max = v;
                break;
            }
        }
        if x_max <= x_min {
            x_max = 0.999;
        }

        let visible: Vec<_> = all_ticks
            .iter()
            .filter(|(v, _)| *v >= x_min && *v <= x_max)
            .collect();

        writeln!(f, "# Logit scale for recall axis")?;
        writeln!(
            f,
            "set nonlinear x via log(x/(1.0-x)) inverse 1.0/(1.0+exp(-x))"
        )?;
        writeln!(f, "set xrange [{}:{}]", x_min, x_max)?;

        let tick_str: Vec<String> = visible
            .iter()
            .map(|(v, label)| format!("\"{}\" {}", label, v))
            .collect();
        writeln!(f, "set xtics ({})", tick_str.join(", "))?;
    } else {
        writeln!(f, "set xrange [{}:1.0]", data_min)?;
    }

    writeln!(f)?;

    // Write data blocks
    for (i, s) in data.series.iter().enumerate() {
        // Pareto points
        writeln!(f, "$pareto_{i} << EOD")?;
        for p in &s.pareto_points {
            writeln!(f, "{} {}", p.recall, p.y)?;
        }
        writeln!(f, "EOD")?;

        // Non-Pareto points
        if !opts.pareto_only && !s.other_points.is_empty() {
            writeln!(f, "$other_{i} << EOD")?;
            for p in &s.other_points {
                writeln!(f, "{} {}", p.recall, p.y)?;
            }
            writeln!(f, "EOD")?;
        }
    }

    writeln!(f)?;

    // Plot commands
    let mut plot_cmds = Vec::new();

    for (i, s) in data.series.iter().enumerate() {
        let (r, g, b) = COLORS[i % COLORS.len()];
        let color = format!("#{r:02x}{g:02x}{b:02x}");

        if !opts.pareto_only && !s.other_points.is_empty() {
            plot_cmds.push(format!(
                "$other_{i} using 1:2 with points pt 7 ps 0.5 lc rgb '{color}' notitle"
            ));
        }

        plot_cmds.push(format!(
            "$pareto_{i} using 1:2 with linespoints lw 2.5 pt 7 ps 1.0 lc rgb '{color}' title '{name}'",
            name = s.name,
        ));
    }

    writeln!(f, "plot \\")?;
    for (i, cmd) in plot_cmds.iter().enumerate() {
        if i < plot_cmds.len() - 1 {
            writeln!(f, "  {cmd}, \\")?;
        } else {
            writeln!(f, "  {cmd}")?;
        }
    }

    if output_path.is_none() {
        writeln!(f, "pause -1 'Press Enter to close'")?;
    }

    Ok(())
}

/// Escape a string for embedding in a *double-quoted, enhanced-text*
/// gnuplot string. Two layers process the text: the string parser (handles
/// `\"`, `\\`, `\n`) and then enhanced text (treats `_ ^ { } & @ ~` as
/// markup — e.g. `_` starts a subscript). To show those characters
/// literally we must get a backslash past the string parser to enhanced,
/// hence the doubled `\\` for metacharacters. The caller adds its own
/// `\n` line breaks and `{/=N ...}` font markup outside this function.
pub(super) fn escape_gp(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            // literal backslash: \\\\ -> string parser -> \\ -> enhanced -> \
            '\\' => out.push_str("\\\\\\\\"),
            // string-literal quote escape
            '"' => out.push_str("\\\""),
            // enhanced-text metacharacters: \\X -> string parser -> \X ->
            // enhanced renders X literally (no subscript/superscript/group)
            '_' | '^' | '{' | '}' | '&' | '@' | '~' => {
                out.push_str("\\\\");
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// The payload for `set title "<...>"`: the main title plus each caption line
/// appended as a smaller enhanced-text line. Shared by the plot renderers.
pub(super) fn title_string(title: &str, caption: Option<&str>) -> String {
    let mut t = escape_gp(title);
    if let Some(c) = caption {
        for line in c.lines() {
            t.push_str("\\n{/=9 ");
            t.push_str(&escape_gp(line));
            t.push('}');
        }
    }
    t
}
