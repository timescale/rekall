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
        for &(r, _) in &s.pareto_points {
            if r < data_min {
                data_min = r;
            }
            if r > data_max {
                data_max = r;
            }
        }
        for &(r, _) in &s.other_points {
            if r < data_min {
                data_min = r;
            }
            if r > data_max {
                data_max = r;
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
    writeln!(f, "set title '{}'", data.title)?;
    writeln!(f, "set xlabel noenhanced 'Recall@10' offset 0,-1")?;
    writeln!(f, "set ylabel 'Queries per second'")?;
    writeln!(f, "set grid")?;
    writeln!(f, "set key left bottom box opaque")?;
    writeln!(f, "set bmargin 7")?;
    writeln!(f, "set xtics rotate by -45")?;
    writeln!(f)?;

    // Y-axis
    if opts.log_y {
        writeln!(f, "set logscale y")?;
        writeln!(f, "set format y '%.0s%cQPS'")?;
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
        for &(recall, qps) in &s.pareto_points {
            writeln!(f, "{recall} {qps}")?;
        }
        writeln!(f, "EOD")?;

        // Non-Pareto points
        if !opts.pareto_only && !s.other_points.is_empty() {
            writeln!(f, "$other_{i} << EOD")?;
            for &(recall, qps) in &s.other_points {
                writeln!(f, "{recall} {qps}")?;
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
