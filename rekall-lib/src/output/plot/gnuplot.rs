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

    let min_r = if opts.min_recall > 0.0 {
        opts.min_recall
    } else {
        0.5
    };

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
    writeln!(f, "set xlabel 'Recall@10' offset 0,-1")?;
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
        writeln!(f, "# Logit scale for recall axis")?;
        writeln!(
            f,
            "set nonlinear x via log(x/(1.0-x)) inverse 1.0/(1.0+exp(-x))"
        )?;
        writeln!(f, "set xrange [{}:0.999]", min_r)?;
        writeln!(
            f,
            "set xtics (\"0.5\" 0.5, \"0.6\" 0.6, \"0.7\" 0.7, \"0.8\" 0.8, \
             \"0.85\" 0.85, \"0.9\" 0.9, \"0.925\" 0.925, \"0.95\" 0.95, \
             \"0.975\" 0.975, \"0.99\" 0.99, \"0.995\" 0.995, \"0.999\" 0.999)"
        )?;
    } else {
        writeln!(f, "set xrange [{}:1.0]", min_r)?;
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
