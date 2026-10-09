// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

//! GitHub Pages side of publishing: push per-run report pages and a catalog
//! to the repo's `gh-pages` branch using the deploy key the instance already
//! has configured (`git config core.sshCommand`).
//!
//! Layout on the branch:
//! ```text
//!   index.html                      # catalog, grouped by dataset
//!   .nojekyll
//!   runs/<dataset>/<date>/<inst>-<hhmmss>/
//!       index.html                  # full run metadata + plot links
//!       meta.json                   # catalog source / completion marker
//!       <dataset>-report-*.html|svg # copied from the results dir
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::PublishConfig;
use crate::error::{Error, Result};
use crate::metrics::RunSummary;

use super::compare::{self, Cell, ColumnMode};
use super::report;
use super::summary::{read_summary_json, write_summary_json, CompactSummary, RecallBasis};
use super::RunMeta;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Prettify a dataset slug for display: `cohere-5m` -> `Cohere-5M`.
fn pretty_dataset(d: &str) -> String {
    let mut out = String::new();
    let mut prev_digit = false;
    for (i, c) in d.chars().enumerate() {
        if i == 0 || (prev_digit && c.is_ascii_alphabetic()) {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
        prev_digit = c.is_ascii_digit();
    }
    out
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Run `git` in `work` (optionally with a deploy-key SSH command), returning
/// trimmed stdout or an error carrying stderr.
fn git(work: &Path, ssh: Option<&str>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(work).args(args);
    if let Some(s) = ssh {
        cmd.env("GIT_SSH_COMMAND", s);
    }
    let out = cmd
        .output()
        .map_err(|e| Error::Config(format!("git {args:?}: {e}")))?;
    if !out.status.success() {
        return Err(Error::Config(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Origin URL and configured SSH command of the local rekall checkout.
fn repo_origin(repo: &Path) -> Result<(String, Option<String>)> {
    if !repo.join(".git").exists() {
        return Err(Error::Config(format!(
            "rekall repo checkout not found at {} — set [publish].repo_path",
            repo.display()
        )));
    }
    let url = git(repo, None, &["remote", "get-url", "origin"])?;
    let ssh = git(repo, None, &["config", "core.sshCommand"])
        .ok()
        .filter(|s| !s.is_empty());
    Ok((url, ssh))
}

/// Clone (or refresh) the gh-pages branch into `work`, creating it as an
/// orphan branch if the remote doesn't have it yet.
fn ensure_checkout(work: &Path, url: &str, branch: &str, ssh: Option<&str>) -> Result<()> {
    if work.join(".git").exists() {
        git(work, None, &["remote", "set-url", "origin", url]).ok();
        if git(work, ssh, &["fetch", "origin", branch]).is_ok() {
            git(
                work,
                None,
                &["checkout", "-B", branch, &format!("origin/{branch}")],
            )?;
            git(work, ssh, &["reset", "--hard", &format!("origin/{branch}")])?;
        } else {
            ensure_orphan(work, branch)?;
        }
        return Ok(());
    }

    if let Some(parent) = work.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let cloned = {
        let mut c = Command::new("git");
        c.args(["clone", "--branch", branch, "--single-branch", url])
            .arg(work);
        if let Some(s) = ssh {
            c.env("GIT_SSH_COMMAND", s);
        }
        c.output().map(|o| o.status.success()).unwrap_or(false)
    };
    if !cloned {
        // Branch doesn't exist remotely: clone the default branch, then orphan.
        let mut c = Command::new("git");
        c.args(["clone", url]).arg(work);
        if let Some(s) = ssh {
            c.env("GIT_SSH_COMMAND", s);
        }
        let o = c
            .output()
            .map_err(|e| Error::Config(format!("git clone {url}: {e}")))?;
        if !o.status.success() {
            return Err(Error::Config(format!(
                "git clone {url} failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )));
        }
        ensure_orphan(work, branch)?;
    }
    Ok(())
}

fn ensure_orphan(work: &Path, branch: &str) -> Result<()> {
    git(work, None, &["checkout", "--orphan", branch])?;
    // Drop everything inherited from the default branch.
    git(work, None, &["rm", "-rf", "--quiet", "."]).ok();
    Ok(())
}

/// Stage, commit, and push the branch. When nothing changed this is a no-op
/// (Ok) — unless `force` is set, in which case it re-commits (empty) and
/// re-pushes anyway, so `--force` always republishes the regenerated
/// artifacts even when they're byte-identical to what's already live.
fn commit_push(work: &Path, branch: &str, ssh: Option<&str>, msg: &str, force: bool) -> Result<()> {
    git(work, None, &["add", "-A"])?;
    let unchanged = git(work, None, &["status", "--porcelain"])?.is_empty();
    if unchanged && !force {
        tracing::info!("gh-pages: nothing changed, skipping commit");
        return Ok(());
    }
    if unchanged {
        tracing::info!("gh-pages: no content changes, forcing republish (--force)");
    }
    let mut commit_args = vec![
        "-c",
        "user.email=rekall@localhost",
        "-c",
        "user.name=rekall",
        "commit",
        "-m",
        msg,
    ];
    if unchanged {
        // Nothing staged, but `--force` asked us to republish anyway.
        commit_args.push("--allow-empty");
    }
    git(work, None, &commit_args)?;
    if git(work, ssh, &["push", "origin", branch]).is_err() {
        // A concurrent publish moved the branch — rebase onto it and retry once.
        git(work, ssh, &["fetch", "origin", branch])?;
        git(work, ssh, &["rebase", &format!("origin/{branch}")]).map_err(|_| {
            Error::Config(
                "gh-pages push rejected and automatic rebase failed; \
                 re-run `rekall publish` to retry"
                    .to_string(),
            )
        })?;
        git(work, ssh, &["push", "origin", branch])?;
    }
    Ok(())
}

/// Publish a run's reports + metadata to GitHub Pages. Returns the public URL
/// of the run's page when `public_base_url` is configured.
#[allow(clippy::too_many_arguments)]
pub fn publish(
    summary: &RunSummary,
    meta: &RunMeta,
    compact: &CompactSummary,
    results_dir: &Path,
    run_id: &str,
    config: &PublishConfig,
    dry_run: bool,
    force: bool,
) -> Result<Option<String>> {
    let report_files = collect_report_files(summary, results_dir);

    if dry_run {
        println!("[dry-run] gh-pages would update runs/{run_id}/:");
        println!("  runs/{run_id}/index.html  (run metadata + tables + plots)");
        println!("  runs/{run_id}/meta.json");
        println!("  runs/{run_id}/summary.json");
        for f in &report_files {
            println!(
                "  runs/{run_id}/{}",
                f.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        println!("  index.html  (catalog, regenerated)");
        return Ok(public_url(config, run_id));
    }

    let repo = expand_home(config.repo_path.as_deref().unwrap_or("~/repos/rekall"));
    let (url, ssh) = repo_origin(&repo)?;
    let branch = &config.pages_branch;

    let work = pages_workdir(&url);
    ensure_checkout(&work, &url, branch, ssh.as_deref())?;

    // `.nojekyll` so GitHub serves the HTML/SVG verbatim.
    std::fs::write(work.join(".nojekyll"), "")
        .map_err(|e| Error::Config(format!("write .nojekyll: {e}")))?;

    // Per-run dir: copy reports, write meta.json + index.html.
    let run_dir = work.join("runs").join(run_id);
    std::fs::create_dir_all(&run_dir)
        .map_err(|e| Error::Config(format!("create {}: {e}", run_dir.display())))?;
    for f in &report_files {
        let name = f.file_name().unwrap_or_default();
        std::fs::copy(f, run_dir.join(name))
            .map_err(|e| Error::Config(format!("copy {}: {e}", f.display())))?;
    }
    let meta_json = serde_json::to_vec_pretty(meta)
        .map_err(|e| Error::Config(format!("serialize meta.json: {e}")))?;
    std::fs::write(run_dir.join("meta.json"), meta_json)
        .map_err(|e| Error::Config(format!("write meta.json: {e}")))?;
    // summary.json feeds the catalog's comparison column (recomputed from it
    // on every catalog regeneration, so the method can evolve).
    write_summary_json(&run_dir, compact)?;
    std::fs::write(
        run_dir.join("index.html"),
        render_run_page(summary, meta, compact, &report_files),
    )
    .map_err(|e| Error::Config(format!("write run index.html: {e}")))?;

    // Regenerate the catalog from every run's meta.json + summary.json.
    let entries = load_all_meta(&work);
    std::fs::write(work.join("index.html"), render_catalog(&entries))
        .map_err(|e| Error::Config(format!("write catalog index.html: {e}")))?;

    commit_push(
        &work,
        branch,
        ssh.as_deref(),
        &format!("publish {run_id}"),
        force,
    )?;
    tracing::info!("published gh-pages run page for {run_id}");
    Ok(public_url(config, run_id))
}

fn public_url(config: &PublishConfig, run_id: &str) -> Option<String> {
    config
        .public_base_url
        .as_ref()
        .map(|base| format!("{}/runs/{run_id}/", base.trim_end_matches('/')))
}

/// Persistent clone location, keyed by repo name, under the user cache dir.
/// `REKALL_PAGES_WORKDIR` overrides the base (used by tests / relocation).
fn pages_workdir(url: &str) -> PathBuf {
    let name = url
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .trim_end_matches(".git");
    let base = if let Ok(over) = std::env::var("REKALL_PAGES_WORKDIR") {
        PathBuf::from(over)
    } else {
        std::env::var("HOME")
            .map(|h| Path::new(&h).join(".cache/rekall/pages"))
            .unwrap_or_else(|_| PathBuf::from("/tmp/rekall-pages"))
    };
    base.join(format!("{name}-gh-pages"))
}

/// Report html + svg files for this run (in display order).
fn collect_report_files(summary: &RunSummary, dir: &Path) -> Vec<PathBuf> {
    let prefix = report::report_prefix(summary);
    let mut out = Vec::new();
    for kind in [
        "pareto",
        "per-config",
        "distance-recall",
        "build-times",
        "build-sizes",
    ] {
        for ext in ["html", "svg"] {
            let p = dir.join(format!("{prefix}-report-{kind}.{ext}"));
            if p.exists() {
                out.push(p);
            }
        }
    }
    out
}

/// One published run as the catalog sees it: identity from meta.json plus,
/// for runs published since summaries exist, the compact numbers behind the
/// comparison column. `summary: None` (old, un-backfilled runs) renders "—".
struct CatalogEntry {
    meta: RunMeta,
    summary: Option<CompactSummary>,
}

fn load_all_meta(work: &Path) -> Vec<CatalogEntry> {
    let mut out = Vec::new();
    let runs = work.join("runs");
    let mut stack = vec![runs];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == "meta.json") {
                if let Ok(s) = std::fs::read_to_string(&p) {
                    if let Ok(m) = serde_json::from_str::<RunMeta>(&s) {
                        out.push(CatalogEntry {
                            meta: m,
                            summary: p.parent().and_then(read_summary_json),
                        });
                    }
                }
            }
        }
    }
    out
}

const STYLE: &str = "body{font-family:system-ui,sans-serif;margin:0;color:#1a1a1a}\
.wrap{max-width:1100px;margin:0 auto;padding:24px}\
h1{font-size:24px;margin:0 0 4px}h2{font-size:19px;margin:28px 0 8px;border-bottom:2px solid #eee;padding-bottom:4px}\
.sub{color:#555;font-size:13px;margin-bottom:16px}\
table{border-collapse:collapse;width:100%;font-size:13px;margin:8px 0}\
th,td{text-align:left;padding:6px 10px;border-bottom:1px solid #eee;vertical-align:top}\
th{color:#555;font-weight:600}\
a{color:#1f77b4;text-decoration:none}a:hover{text-decoration:underline}\
a.svg{font-size:11px;color:#888;border:1px solid #ddd;border-radius:3px;padding:0 4px;margin-left:2px}\
.kv td:first-child{color:#555;width:230px}\
details{margin:8px 0}summary{cursor:pointer;color:#555;font-size:13px}\
pre{background:#f6f8fa;padding:10px;border-radius:6px;overflow:auto;font-size:12px}\
.thumbs img{max-width:520px;border:1px solid #eee;border-radius:6px;margin:8px 8px 0 0}\
.pos{color:#188038}.neg{color:#c5221f}\
.faint{color:#999;font-size:11px}\
tr.sect td{background:#f6f8fa;color:#555;font-weight:600;font-size:12px}";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{}</title>\
         <style>{STYLE}</style></head><body><div class=\"wrap\">{body}</div></body></html>",
        esc(title)
    )
}

fn fmt_thousands(v: f64) -> String {
    let s = format!("{:.0}", v);
    let mut out = String::new();
    let digits: Vec<char> = s.chars().collect();
    for (i, c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) && c.is_ascii_digit() {
            out.push(',');
        }
        out.push(*c);
    }
    out
}

fn fmt_ms(us: f64) -> String {
    format!("{:.2} ms", us / 1000.0)
}

fn fmt_secs(s: f64) -> String {
    if s >= 100.0 {
        format!("{s:.0} s")
    } else {
        format!("{s:.1} s")
    }
}

/// A signed percentage delta, coloured by whether it's an improvement
/// (`lower_is_better` flips the goodness of the sign).
fn fmt_delta(pct: f64, lower_is_better: bool) -> String {
    let good = if lower_is_better {
        pct <= 0.0
    } else {
        pct >= 0.0
    };
    let class = if good { "pos" } else { "neg" };
    format!("<span class=\"{class}\">{pct:+.0}%</span>")
}

/// Row label of the throughput-under-load table: the recall target and
/// client count, with the `search.clients` entry it came from.
fn load_row_label(row: &compare::LoadRow) -> String {
    let clients = match row.clients_spec.as_deref() {
        None => "1 client".to_string(),
        Some(spec) if spec.ends_with('x') => format!(
            "{} clients <span class=\"faint\">({} of {} CPUs)</span>",
            row.clients,
            esc(spec),
            row.cpu_count
        ),
        Some(_) => format!("{} clients", row.clients),
    };
    format!("{:.2} · {clients}", row.target)
}

/// The throughput-under-load table: each column's operating point for a
/// recall target measured with concurrent clients, as wall-clock QPS with
/// the all-clients-busy throughput in parentheses, and the p50 and p95
/// latency below. Empty string when the run has no concurrency
/// measurements.
fn render_load_table(
    compact: &CompactSummary,
    model: &compare::RunTableModel,
    header: &dyn Fn(&mut String, &str),
) -> String {
    let rows = compare::build_load_rows(compact, model);
    let Some(first) = compact.load_points.first() else {
        return String::new();
    };
    if rows.is_empty() {
        return String::new();
    }
    let delta_col = model.prism_col.is_some();
    let mut b = String::new();
    b.push_str("<h2>Throughput under load</h2>");
    header(&mut b, "Recall · clients");
    for row in &rows {
        // Best competitor (or best overall in Configs mode) by wall-clock QPS.
        let best = row
            .cells
            .iter()
            .enumerate()
            .filter(|(i, _)| !(delta_col && Some(*i) == model.prism_col))
            .filter_map(|(i, c)| c.as_ref().map(|c| (i, c.qps_wall)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i);
        b.push_str(&format!("<tr><td>{}</td>", load_row_label(row)));
        for (i, cell) in row.cells.iter().enumerate() {
            match cell {
                Some(c) => {
                    let wall = fmt_thousands(c.qps_wall);
                    let wall = if best == Some(i) {
                        format!("<b>{wall}</b>")
                    } else {
                        wall
                    };
                    let speedup = c
                        .speedup
                        .map(|x| format!(" · {x:.1}\u{d7}"))
                        .unwrap_or_default();
                    b.push_str(&format!(
                        "<td>{wall} <span class=\"faint\">({})</span>{speedup}<br>\
                         p50 {} · p95 {}</td>",
                        fmt_thousands(c.throughput_qps),
                        fmt_ms(c.p50_us),
                        fmt_ms(c.p95_us)
                    ));
                }
                None => b.push_str("<td>—</td>"),
            }
        }
        if delta_col {
            let pair = model
                .prism_col
                .zip(best)
                .and_then(|(m, c)| Some((row.cells[m].as_ref()?, row.cells[c].as_ref()?)));
            match pair {
                Some((m, c)) => b.push_str(&format!(
                    "<td>{}</td>",
                    fmt_delta((m.qps_wall / c.qps_wall - 1.0) * 100.0, false)
                )),
                None => b.push_str("<td>—</td>"),
            }
        }
        b.push_str("</tr>");
    }
    b.push_str("</table>");

    b.push_str(&format!(
        "<p class=\"sub\">Each configuration is measured at its operating point \
         for the target: its fastest sweep point whose recall reaches it. Values \
         are measured there, not interpolated, so the recall can be above the \
         target. Each cell is wall-clock QPS over the whole run, then in \
         parentheses the throughput while every client is busy, which leaves \
         out the drain at the end, and below them the p50 and p95 query \
         latency over all clients. With more than one client, N\u{d7} is the \
         speedup: wall-clock QPS over the same configuration's single-client \
         wall-clock QPS at that target. Ideal scaling matches the client count \
         until the host's CPUs saturate, after which it flattens. Clients are concurrent connections from \
         rekall, and CPU counts are the rekall host's. Wall-clock QPS includes \
         rekall's own work between queries, so the 1-client row reads a little \
         below QPS at recall, which comes from query latency. Recall is {}.</p>",
        esc(first.recall_basis.describe())
    ));
    // One note per column, target and config. Client counts are named only
    // when a column's best config differs between them.
    let mut groups: Vec<(usize, f64, &str, f64, Vec<usize>)> = Vec::new();
    for row in &rows {
        for (i, cell) in row.cells.iter().enumerate() {
            let Some(c) = cell else { continue };
            match groups
                .iter_mut()
                .find(|g| g.0 == i && g.1 == row.target && g.2 == c.config)
            {
                Some(g) => g.4.push(row.clients),
                None => groups.push((i, row.target, &c.config, c.recall, vec![row.clients])),
            }
        }
    }
    let notes: Vec<String> = groups
        .iter()
        .map(|(i, target, config, recall, clients)| {
            let shared = groups
                .iter()
                .filter(|g| g.0 == *i && g.1 == *target)
                .count()
                > 1;
            let which = if shared {
                let counts: Vec<String> = clients.iter().map(|n| n.to_string()).collect();
                format!(" with {} clients", counts.join(", "))
            } else {
                String::new()
            };
            format!(
                "{} @{target:.2}{which}: {config} (r={recall:.3})",
                model.columns[*i]
            )
        })
        .collect();
    b.push_str(&format!(
        "<p class=\"sub\">Measured on config — {}</p>",
        esc(&notes.join(" · "))
    ));
    b
}

/// The three run-page summary tables (QPS at recall, latency at recall,
/// index build), rendered from the compact summary. Empty string when the
/// run recorded no search points.
fn render_run_tables(compact: &CompactSummary) -> String {
    let Some(model) = compare::build_table_model(compact, compare::RECALL_TARGETS) else {
        return String::new();
    };
    let mut b = String::new();
    let delta_col = model.prism_col.is_some();

    let header = |b: &mut String, first: &str| {
        b.push_str("<table><tr>");
        b.push_str(&format!("<th>{}</th>", esc(first)));
        for c in &model.columns {
            b.push_str(&format!("<th>{}</th>", esc(c)));
        }
        if delta_col {
            b.push_str("<th>\u{394} vs best competitor</th>");
        }
        b.push_str("</tr>");
    };

    // Best competitor (or, in Configs mode, best overall) column of a row by
    // a metric; `lower_is_better` picks min instead of max. Only interpolated
    // cells compete — every value in a row is at the identical recall.
    let best_col = |cells: &[Cell], metric: &dyn Fn(&compare::InterpCell) -> f64, lower: bool| {
        let candidates = cells.iter().enumerate().filter_map(|(i, c)| {
            if delta_col && Some(i) == model.prism_col {
                return None; // "best" means best competitor in Families mode
            }
            match c {
                Cell::Interpolated(v) => Some((i, metric(v))),
                _ => None,
            }
        });
        if lower {
            candidates
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        } else {
            candidates
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        }
    };

    let delta_cell = |cells: &[Cell],
                      best: Option<usize>,
                      metric: &dyn Fn(&compare::InterpCell) -> f64,
                      lower: bool| {
        let (Some(mc), Some(bc)) = (model.prism_col, best) else {
            return "<td>—</td>".to_string();
        };
        let (Cell::Interpolated(m), Cell::Interpolated(c)) = (&cells[mc], &cells[bc]) else {
            return "<td>—</td>".to_string();
        };
        let pct = (metric(m) / metric(c) - 1.0) * 100.0;
        format!("<td>{}</td>", fmt_delta(pct, lower))
    };

    // A non-interpolable cell, annotated with which side of the target the
    // column's measurements sit on.
    let dash_cell = |cell: &Cell| match cell {
        Cell::Below { max_recall } => {
            format!("<td>— <span class=\"faint\">(max r={max_recall:.3})</span></td>")
        }
        Cell::Above { min_recall } => {
            format!("<td>— <span class=\"faint\">(min r={min_recall:.3})</span></td>")
        }
        // Sweeps surround the target but none spans it.
        Cell::Gap => "<td>—</td>".to_string(),
        Cell::Interpolated(_) => unreachable!("interpolated cells are rendered by the caller"),
    };

    // --- QPS at recall -----------------------------------------------------
    b.push_str("<h2>QPS at recall</h2>");
    header(&mut b, "Recall");
    for row in &model.rows {
        let qps = |v: &compare::InterpCell| v.qps;
        let best = best_col(&row.cells, &qps, false);
        b.push_str(&format!("<tr><td>{:.2}</td>", row.target));
        for (i, cell) in row.cells.iter().enumerate() {
            match cell {
                Cell::Interpolated(v) => {
                    let s = fmt_thousands(v.qps);
                    if best == Some(i) {
                        b.push_str(&format!("<td><b>{s}</b></td>"));
                    } else {
                        b.push_str(&format!("<td>{s}</td>"));
                    }
                }
                other => b.push_str(&dash_cell(other)),
            }
        }
        if delta_col {
            b.push_str(&delta_cell(&row.cells, best, &qps, false));
        }
        b.push_str("</tr>");
    }
    b.push_str("</table>");

    // --- Throughput under load ---------------------------------------------
    b.push_str(&render_load_table(compact, &model, &header));

    // --- Latency at recall -------------------------------------------------
    b.push_str("<h2>Latency at recall</h2>");
    header(&mut b, "");
    type LatencyMetric = fn(&compare::InterpCell) -> f64;
    let percentiles: [(&str, LatencyMetric); 3] = [
        ("p50", |v| v.p50_us),
        ("p95", |v| v.p95_us),
        ("p99", |v| v.p99_us),
    ];
    for row in &model.rows {
        // Section header: one group of percentile rows per recall target.
        b.push_str(&format!(
            "<tr class=\"sect\"><td>@ recall {:.2}</td>",
            row.target
        ));
        for _ in &row.cells {
            b.push_str("<td></td>");
        }
        if delta_col {
            b.push_str("<td></td>");
        }
        b.push_str("</tr>");

        for (name, metric) in &percentiles {
            let m = |v: &compare::InterpCell| metric(v);
            let best = best_col(&row.cells, &m, true);
            b.push_str(&format!("<tr><td>{name}</td>"));
            for (i, cell) in row.cells.iter().enumerate() {
                match cell {
                    Cell::Interpolated(v) => {
                        let s = fmt_ms(metric(v));
                        if best == Some(i) {
                            b.push_str(&format!("<td><b>{s}</b></td>"));
                        } else {
                            b.push_str(&format!("<td>{s}</td>"));
                        }
                    }
                    _ => b.push_str("<td>—</td>"),
                }
            }
            if delta_col {
                b.push_str(&delta_cell(&row.cells, best, &m, true));
            }
            b.push_str("</tr>");
        }
    }
    b.push_str("</table>");

    // Shared caption for the two search tables.
    b.push_str(&format!(
        "<p class=\"sub\">All values are taken at exactly the stated recall: \
         each configuration's own sweep is log-interpolated between its two \
         surrounding measured points (measured values are used verbatim on \
         an exact hit) and the best configuration at that recall is shown — \
         values are never interpolated across different configurations. \
         \u{201c}—\u{201d} means no single sweep spans that recall; nothing \
         is extrapolated. Recall is {}. Insert/update/delete workloads are \
         not measured.</p>",
        esc(compact.recall_basis.describe())
    ));
    if model.mode == ColumnMode::Families {
        let mut notes = Vec::new();
        for row in &model.rows {
            for (i, cell) in row.cells.iter().enumerate() {
                if let Cell::Interpolated(v) = cell {
                    notes.push(format!(
                        "{} @{:.2}: {}",
                        model.columns[i], row.target, v.config
                    ));
                }
            }
        }
        if !notes.is_empty() {
            b.push_str(&format!(
                "<p class=\"sub\">Interpolated on config — {}</p>",
                esc(&notes.join(" · "))
            ));
        }
    }

    // --- Index build ---------------------------------------------------------
    if model.build_cells.iter().any(Option::is_some) {
        b.push_str("<h2>Build comparison</h2>");
        header(&mut b, "");
        type BuildMetric = (
            &'static str,
            fn(&compare::BuildCell) -> f64,
            fn(&compare::BuildCell) -> String,
        );
        let rows: [BuildMetric; 2] = [
            (
                "Build time",
                |c| c.build_time_secs,
                |c| fmt_secs(c.build_time_secs),
            ),
            (
                "Index size",
                |c| c.size_bytes as f64,
                |c| human_bytes(c.size_bytes),
            ),
        ];
        for (name, metric, fmt) in &rows {
            let best = {
                let candidates = model.build_cells.iter().enumerate().filter_map(|(i, c)| {
                    if delta_col && Some(i) == model.prism_col {
                        return None;
                    }
                    c.as_ref().map(|c| (i, metric(c)))
                });
                candidates
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(i, _)| i)
            };
            b.push_str(&format!("<tr><td>{name}</td>"));
            for (i, cell) in model.build_cells.iter().enumerate() {
                match cell {
                    Some(c) => {
                        let v = fmt(c);
                        if best == Some(i) {
                            b.push_str(&format!("<td><b>{v}</b></td>"));
                        } else {
                            b.push_str(&format!("<td>{v}</td>"));
                        }
                    }
                    None => b.push_str("<td>—</td>"),
                }
            }
            if delta_col {
                let cell = match (
                    model.prism_col.and_then(|i| model.build_cells[i].as_ref()),
                    best.and_then(|i| model.build_cells[i].as_ref()),
                ) {
                    (Some(m), Some(c)) => {
                        let pct = (metric(m) / metric(c) - 1.0) * 100.0;
                        format!("<td>{}</td>", fmt_delta(pct, true))
                    }
                    _ => "<td>—</td>".to_string(),
                };
                b.push_str(&cell);
            }
            b.push_str("</tr>");
        }
        b.push_str("</table>");

        // Which build each family's numbers refer to (+ cache state). In
        // Configs mode the column name IS the config, so no note is needed.
        if model.mode == ColumnMode::Families {
            let mut notes = Vec::new();
            for (i, cell) in model.build_cells.iter().enumerate() {
                if let Some(c) = cell {
                    let cache = c
                        .cache
                        .as_deref()
                        .map(|s| format!(", {s}"))
                        .unwrap_or_default();
                    notes.push(format!("{} = {}{}", model.columns[i], c.index_name, cache));
                }
            }
            b.push_str(&format!(
                "<p class=\"sub\">Build stats are for the configuration backing \
                 each column's best operating point: {}.</p>",
                esc(&notes.join(" · "))
            ));
        }
    }

    b
}

/// Full per-run metadata page.
fn render_run_page(
    summary: &RunSummary,
    meta: &RunMeta,
    compact: &CompactSummary,
    report_files: &[PathBuf],
) -> String {
    let ds = pretty_dataset(&meta.dataset);
    let mut b = String::new();

    b.push_str("<p class=\"sub\"><a href=\"../../../../index.html\">← all runs</a></p>");
    b.push_str(&format!("<h1>{} — {}</h1>", esc(&ds), esc(&meta.date)));
    b.push_str(&format!(
        "<div class=\"sub\">{} · peak recall {:.4} @ {:.0} QPS</div>",
        esc(meta.instance_type.as_deref().unwrap_or(&meta.hostname)),
        meta.peak_recall,
        meta.qps_at_peak
    ));
    if let Some(comment) = &meta.comment {
        b.push_str(&format!(
            "<div class=\"sub\">\u{201c}{}\u{201d}</div>",
            esc(comment)
        ));
    }

    // Comparison summary tables (QPS / latency at recall targets, builds).
    b.push_str(&render_run_tables(compact));

    // Plots — link only the report files that were actually generated for this
    // run (distance-recall, e.g., only exists if a ground-truth recompute ran).
    b.push_str("<h2>Plots</h2>");
    let prefix = report::report_prefix(summary);
    let present: std::collections::HashSet<String> = report_files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
        .collect();
    let has = |kind: &str, ext: &str| present.contains(&format!("{prefix}-report-{kind}.{ext}"));

    // The label links to the interactive HTML; a small "svg" link beside it
    // offers the static SVG for download (when present).
    // Distance-recall (ann-benchmarks method, tie-aware) is the primary recall
    // view, so it leads the non-pareto plots; per-config is the ID-based view.
    let links: Vec<String> = [
        ("pareto", "Pareto"),
        ("distance-recall", "Distance-recall"),
        ("per-config", "Per-config"),
        ("io", "Disk I/O"),
        ("build-times", "Build times"),
        ("build-sizes", "Index sizes"),
    ]
    .iter()
    .filter(|(kind, _)| has(kind, "html"))
    .map(|(kind, label)| {
        let html = esc(&format!("{prefix}-report-{kind}.html"));
        if has(kind, "svg") {
            let svg = esc(&format!("{prefix}-report-{kind}.svg"));
            format!("<a href=\"{html}\">{label}</a> <a href=\"{svg}\" class=\"svg\">svg</a>")
        } else {
            format!("<a href=\"{html}\">{label}</a>")
        }
    })
    .collect();

    if links.is_empty() {
        b.push_str("<p class=\"sub\">(no plots for this run)</p>");
    } else {
        b.push_str(&links.join(" &nbsp;·&nbsp; "));
        b.push_str("<div class=\"thumbs\">");
        // Recall thumbnail: prefer distance-recall (the correct recall);
        // fall back to per-config when distance-recall wasn't generated.
        let recall_thumb = if has("distance-recall", "svg") {
            "distance-recall"
        } else {
            "per-config"
        };
        for kind in ["pareto", recall_thumb, "build-times", "build-sizes"] {
            if has(kind, "svg") {
                let svg = format!("{prefix}-report-{kind}.svg");
                // The SVG is just the preview image; clicking opens the
                // interactive HTML plot (deselectable lines), falling back to
                // the SVG only if the HTML wasn't generated.
                let target = if has(kind, "html") {
                    format!("{prefix}-report-{kind}.html")
                } else {
                    svg.clone()
                };
                b.push_str(&format!(
                    "<a href=\"{target}\"><img src=\"{svg}\" alt=\"{kind}\"></a>",
                    target = esc(&target),
                    svg = esc(&svg)
                ));
            }
        }
        b.push_str("</div>");
    }

    // Host / hardware
    b.push_str("<h2>Host</h2><table class=\"kv\">");
    let h = summary.environment.as_ref().map(|e| &e.host);
    row(
        &mut b,
        "Instance type",
        meta.instance_type.as_deref().unwrap_or("—"),
    );
    row(&mut b, "Hostname", &meta.hostname);
    row(
        &mut b,
        "CPU",
        &format!("{} ({} vCPU)", meta.cpu_model, meta.cpu_count),
    );
    row(&mut b, "Memory", &format!("{:.0} GB", meta.memory_gb));
    if let Some(h) = h {
        row(&mut b, "OS", &h.os);
        row(&mut b, "Kernel", &h.kernel);
    }
    b.push_str("</table>");

    // Versions
    b.push_str("<h2>Versions</h2><table class=\"kv\">");
    if let Some(env) = &summary.environment {
        let rk = &env.rekall;
        let commit = rk.git_commit.as_deref().unwrap_or("—");
        let branch = rk.git_branch.as_deref().unwrap_or("—");
        row(
            &mut b,
            "rekall",
            &format!("{} ({commit} / {branch})", rk.version),
        );
    }
    if let Some(pg) = &meta.pg_version {
        row(&mut b, "PostgreSQL", pg);
    }
    for (name, tag) in &meta.extensions {
        row(&mut b, name, tag);
    }
    // The pg_vectorsearch branch the run was built from (the commit above is
    // authoritative; this is the readable ref).
    if let Some(branch) = summary
        .config
        .get("pg_vectorsearch")
        .and_then(|m| m.get("branch"))
        .and_then(|v| v.as_str())
    {
        row(&mut b, "pg_vectorsearch branch", branch);
    }
    b.push_str("</table>");

    // Search config
    b.push_str("<h2>Search</h2><table class=\"kv\">");
    let sc = &summary.config["search"];
    for k in ["k", "runs", "warmup", "num_queries"] {
        if let Some(v) = sc.get(k) {
            row(&mut b, k, &v.to_string());
        }
    }
    // S3 archive: show the s3:// path (copy-pasteable for `aws s3 cp`) linked
    // to the S3 console (reachable for viewers with AWS access; others get the
    // AWS login). A direct object URL would 404/AccessDenied on a private bucket.
    let s3_cell = match s3_console_url(&meta.s3_url) {
        Some(url) => format!("<a href=\"{}\">{}</a>", esc(&url), esc(&meta.s3_url)),
        None => esc(&meta.s3_url),
    };
    b.push_str(&format!("<tr><td>S3 archive</td><td>{s3_cell}</td></tr>"));
    b.push_str("</table>");

    // Index builds
    if !summary.index_builds.is_empty() {
        b.push_str(
            "<h2>Index builds</h2><table><tr><th>Index</th><th>Type</th>\
                    <th>Build time</th><th>Size</th><th>Workers</th></tr>",
        );
        for ib in &summary.index_builds {
            b.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{:.1}s</td><td>{}</td><td>{}</td></tr>",
                esc(&ib.index_name),
                esc(&ib.index_type),
                ib.build_time.as_secs_f64(),
                human_bytes(ib.index_size_bytes),
                ib.parallel_workers
            ));
        }
        b.push_str("</table>");
        // Full per-index params, collapsible.
        b.push_str("<details><summary>Index parameters</summary><pre>");
        for ib in &summary.index_builds {
            b.push_str(&esc(&format!(
                "{}: {}\n",
                ib.index_name,
                serde_json::to_string(&ib.params).unwrap_or_default()
            )));
        }
        b.push_str("</pre></details>");
    }

    // Observed instance settings, collapsible. Sits beside the index
    // parameters and PG settings because it answers the same kind of
    // question -- what was this measured on -- and because a config file
    // cannot answer it: `disk_iops = 16000` was silently dropped by the
    // native launch path and every such run used the gp3 3000 baseline with
    // nothing on the page to say so.
    if let Some(env) = &summary.environment {
        let h = &env.host;
        b.push_str("<details><summary>Instance settings (observed)</summary><pre>");
        if let Some(t) = &h.instance_type {
            b.push_str(&esc(&format!("instance type          {t}\n")));
        }
        b.push_str(&esc(&format!("vCPU (observed)        {}\n", h.cpu_count)));
        b.push_str(&esc(&format!(
            "memory (observed)      {:.1} GiB\n",
            h.memory_gb
        )));
        b.push_str(&esc(&format!("cpu model              {}\n", h.cpu_model)));
        b.push_str(&esc(&format!("kernel                 {}\n", h.kernel)));
        if let Some(st) = &h.storage {
            b.push_str(&esc(&format!(
                "data directory         {}\n",
                st.data_directory
                    .as_deref()
                    .unwrap_or("(unknown; root volume assumed)")
            )));
            if let Some(m) = &st.mount_point {
                b.push_str(&esc(&format!("mount point            {m}\n")));
            }
            if let Some(f) = &st.filesystem {
                b.push_str(&esc(&format!("filesystem             {f}\n")));
            }
            b.push_str(&esc(&format!("media                  {}\n", st.media)));
            b.push_str(&esc(&format!(
                "device                 {}{}\n",
                st.device,
                if st.device_resolved_from_pgdata {
                    ""
                } else {
                    " (whole-disk guess)"
                }
            )));
            if let Some(g) = st.size_gb {
                b.push_str(&esc(&format!("device size            {g:.0} GB\n")));
            }
            if let Some(v) = &st.ebs_volume_id {
                b.push_str(&esc(&format!("ebs volume             {v}\n")));
            }
            if let Some(t) = &st.ebs_volume_type {
                b.push_str(&esc(&format!("ebs volume type        {t}\n")));
            }
            // Provisioned, not achieved -- the achieved rate is in the
            // per-point disk samples. Absence is printed as the reason it is
            // absent, so a missing number is never read as a default one.
            match st.ebs_provisioned_iops {
                Some(i) => b.push_str(&esc(&format!("provisioned IOPS       {i}\n"))),
                None => b.push_str(&esc(&format!(
                    "provisioned IOPS       — {}\n",
                    st.ebs_source
                ))),
            }
            if let Some(t) = st.ebs_provisioned_throughput_mbs {
                b.push_str(&esc(&format!("provisioned throughput {t} MB/s\n")));
            }
            if let Some(sc) = &st.scheduler {
                b.push_str(&esc(&format!("io scheduler           {sc}\n")));
            }
            if let Some(n) = st.nr_requests {
                b.push_str(&esc(&format!("nr_requests            {n}\n")));
            }
            if let Some(r) = st.read_ahead_kb {
                b.push_str(&esc(&format!("read_ahead_kb          {r}\n")));
            }
        } else {
            b.push_str(&esc("storage                — not observed\n"));
        }
        b.push_str("</pre></details>");
    }

    // Full PG settings, collapsible.
    if let Some(pg) = &summary.pg_runtime {
        b.push_str("<details><summary>PostgreSQL settings</summary><pre>");
        for (k, v) in &pg.settings {
            b.push_str(&esc(&format!("{k} = {v}\n")));
        }
        b.push_str("</pre></details>");
    }

    page(&format!("{ds} — {}", meta.date), &b)
}

fn row(b: &mut String, k: &str, v: &str) {
    b.push_str(&format!("<tr><td>{}</td><td>{}</td></tr>", esc(k), esc(v)));
}

/// AWS S3 console URL for an `s3://bucket/prefix/` location — clickable for
/// viewers with access to the account (others get the AWS login). Returns
/// `None` if the string isn't an `s3://` URL.
fn s3_console_url(s3_url: &str) -> Option<String> {
    let rest = s3_url.strip_prefix("s3://")?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    Some(format!(
        "https://s3.console.aws.amazon.com/s3/buckets/{bucket}?prefix={prefix}"
    ))
}

fn human_bytes(n: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

/// The catalog's PRISM-vs-competition cell: PRISM's QPS gain/loss at the
/// low / middle / high recall of each competitor's common frontier range.
/// "—" when there is nothing to compare: no summary.json (old run), a
/// single-family run, no frontier overlap — or an ID-based run, so the
/// column never mixes recall bases (distance-based runs are the norm; the
/// ID fallback only happens when ground truth was unavailable at publish).
fn comparison_cell(summary: Option<&CompactSummary>) -> String {
    let Some(cs) = summary else {
        return "—".to_string();
    };
    if cs.recall_basis != RecallBasis::Distance {
        return "—".to_string();
    }
    let deltas = compare::prism_comparison(cs);
    let mut lines = Vec::new();
    for d in &deltas {
        if d.samples.is_empty() {
            continue;
        }
        let samples: Vec<String> = d
            .samples
            .iter()
            .map(|s| format!("{} @{:.2}", fmt_delta(s.pct, false), s.recall))
            .collect();
        lines.push(format!(
            "vs {}: {}",
            esc(&d.competitor),
            samples.join(" · ")
        ));
    }
    if lines.is_empty() {
        "—".to_string()
    } else {
        lines.join("<br>")
    }
}

/// Top-level catalog grouped by dataset, newest run first.
fn render_catalog(entries: &[CatalogEntry]) -> String {
    use std::collections::BTreeMap;
    let mut by_ds: BTreeMap<String, Vec<&CatalogEntry>> = BTreeMap::new();
    for e in entries {
        by_ds.entry(e.meta.dataset.clone()).or_default().push(e);
    }

    let mut b = String::new();
    b.push_str("<h1>Rekall benchmark runs</h1>");
    b.push_str(&format!(
        "<div class=\"sub\">{} runs across {} datasets</div>",
        entries.len(),
        by_ds.len()
    ));

    for (dataset, mut runs) in by_ds {
        // newest first by (date, time)
        runs.sort_by(|a, b| (&b.meta.date, &b.meta.time).cmp(&(&a.meta.date, &a.meta.time)));
        b.push_str(&format!("<h2>{}</h2>", esc(&pretty_dataset(&dataset))));
        b.push_str(
            "<table><tr><th>Date</th><th>Comment</th><th>Instance</th>\
             <th>PRISM vs competition</th></tr>",
        );
        for e in runs {
            let m = &e.meta;
            let run_path = format!("runs/{}", m.run_id);
            let inst = m.instance_type.as_deref().unwrap_or(&m.hostname);
            b.push_str(&format!(
                "<tr><td><a href=\"{rp}/index.html\">{date} · {time}</a></td>\
                 <td>{comment}</td>\
                 <td>{inst}</td><td>{cmp}</td></tr>",
                rp = esc(&run_path),
                date = esc(&m.date),
                time = esc(&m.time),
                comment = esc(m.comment.as_deref().unwrap_or("—")),
                inst = esc(inst),
                cmp = comparison_cell(e.summary.as_ref()),
            ));
        }
        b.push_str("</table>");
    }
    page("Rekall benchmark runs", &b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publish::summary::{BuildInfo, SweepPoint};
    use std::collections::BTreeMap;

    fn sweep(name: &str, index_type: &str, recall: f64, qps: f64) -> SweepPoint {
        SweepPoint {
            index_name: name.into(),
            index_type: index_type.into(),
            data_type: None,
            recall,
            qps,
            p50_latency_us: 1e6 / qps * 0.9,
            p95_latency_us: 1e6 / qps * 1.5,
            p99_latency_us: 1e6 / qps * 2.0,
        }
    }

    fn compact(points: Vec<SweepPoint>, builds: Vec<BuildInfo>) -> CompactSummary {
        CompactSummary {
            recall_basis: RecallBasis::Distance,
            points,
            builds,
            load_points: Vec::new(),
        }
    }

    fn empty_compact() -> CompactSummary {
        compact(Vec::new(), Vec::new())
    }

    fn versus_compact() -> CompactSummary {
        compact(
            vec![
                sweep("prism-n16000", "prism", 0.90, 3000.0),
                sweep("prism-n16000", "prism", 0.96, 1500.0),
                sweep("hnsw-m24", "pgvector-hnsw", 0.90, 2000.0),
                sweep("hnsw-m24", "pgvector-hnsw", 0.94, 1200.0),
            ],
            vec![
                BuildInfo {
                    index_name: "prism-n16000".into(),
                    index_type: "prism".into(),
                    build_time_secs: 312.0,
                    size_bytes: 3_100_000_000,
                    cache: Some("prewarmed".into()),
                },
                BuildInfo {
                    index_name: "hnsw-m24".into(),
                    index_type: "pgvector-hnsw".into(),
                    build_time_secs: 508.0,
                    size_bytes: 5_400_000_000,
                    cache: None,
                },
            ],
        )
    }

    fn entry(meta: RunMeta) -> CatalogEntry {
        CatalogEntry {
            meta,
            summary: None,
        }
    }

    fn meta(dataset: &str, date: &str, time: &str) -> RunMeta {
        RunMeta {
            run_id: format!("{dataset}/{date}/i-{}", time.replace(':', "")),
            dataset: dataset.to_string(),
            date: date.to_string(),
            time: time.to_string(),
            timestamp: format!("{date}T{time}+0000"),
            instance_type: Some("r8i.2xlarge".into()),
            hostname: "h".into(),
            cpu_model: "Xeon".into(),
            cpu_count: 8,
            memory_gb: 62.0,
            pg_version: Some("PostgreSQL 18.4".into()),
            rekall_commit: Some("65df24d".into()),
            extensions: BTreeMap::new(),
            peak_recall: 0.987,
            qps_at_peak: 123.0,
            num_indexes: 2,
            s3_url: "s3://rekall/runs/x/".into(),
            comment: None,
        }
    }

    #[test]
    fn pretty_dataset_capitalizes() {
        assert_eq!(pretty_dataset("cohere-5m"), "Cohere-5M");
        assert_eq!(pretty_dataset("cohere-10m"), "Cohere-10M");
        assert_eq!(pretty_dataset("glove-100"), "Glove-100");
    }

    #[test]
    fn catalog_groups_by_dataset_and_links_runs() {
        let entries = vec![
            entry(meta("cohere-5m", "2026-06-10", "09:56:42")),
            entry(meta("cohere-5m", "2026-06-11", "10:00:00")),
            entry(meta("cohere-10m", "2026-06-09", "08:00:00")),
        ];
        let html = render_catalog(&entries);
        assert!(html.contains("<h2>Cohere-5M</h2>"));
        assert!(html.contains("<h2>Cohere-10M</h2>"));
        // newest cohere-5m run (06-11) appears before the older (06-10)
        let p11 = html.find("2026-06-11").unwrap();
        let p10 = html.find("2026-06-10").unwrap();
        assert!(p11 < p10);
        // deep link to a run page
        assert!(html.contains("runs/cohere-5m/2026-06-10/i-095642/index.html"));
        // The dropped columns are gone: everything they carried is one click
        // away on the run page.
        assert!(!html.contains("Peak recall"));
        assert!(!html.contains("<th>Commit</th>"));
        assert!(!html.contains("<th>Plots</th>"));
        assert!(!html.contains("report-pareto.html"));
        assert!(!html.contains("s3.console.aws.amazon.com"));
        // The comparison column exists; runs without summary.json show "—".
        assert!(html.contains("<th>PRISM vs competition</th>"));
        assert!(html.contains("<td>—</td>"));
    }

    #[test]
    fn catalog_comparison_column_from_summary() {
        let mut with = entry(meta("cohere-5m", "2026-06-10", "09:56:42"));
        with.summary = Some(versus_compact());
        let html = render_catalog(&[with]);
        assert!(html.contains("vs pgvector HNSW:"));
        assert!(html.contains("class=\"pos\"")); // prism wins -> green
    }

    #[test]
    fn catalog_comparison_never_mixes_recall_bases() {
        // An ID-based run must not slip an incomparable number into an
        // otherwise distance-based column.
        let mut id_run = entry(meta("cohere-5m", "2026-06-10", "09:56:42"));
        let mut cs = versus_compact();
        cs.recall_basis = RecallBasis::Id;
        id_run.summary = Some(cs);
        let html = render_catalog(&[id_run]);
        assert!(!html.contains("vs pgvector HNSW:"));
        assert!(html.contains("<td>—</td>"));
    }

    #[test]
    fn catalog_comparison_dash_for_single_family() {
        let mut e = entry(meta("cohere-5m", "2026-06-10", "09:56:42"));
        e.summary = Some(compact(
            vec![sweep("prism-a", "prism", 0.9, 1000.0)],
            vec![],
        ));
        let html = render_catalog(&[e]);
        // Nothing to compare against: the cell is a dash, no delta spans.
        assert!(html.contains("<td>—</td>"));
        assert!(!html.contains("class=\"pos\""));
        assert!(!html.contains("class=\"neg\""));
    }

    #[test]
    fn catalog_shows_run_comment() {
        let mut with = meta("cohere-5m", "2026-06-10", "09:56:42");
        with.comment = Some("testing new feature X".into());
        let without = meta("cohere-5m", "2026-06-11", "10:00:00");
        let html = render_catalog(&[entry(with), entry(without)]);
        assert!(html.contains("<th>Comment</th>"));
        assert!(html.contains("<td>testing new feature X</td>"));
        // Runs without a comment render a placeholder.
        assert!(html.contains("<td>—</td>"));
    }

    #[test]
    fn run_page_shows_run_comment() {
        let mut m = meta("cohere-5m", "2026-06-10", "09:56:42");
        m.comment = Some("testing new feature X".into());
        let html = render_run_page(&summary_for(&m), &m, &empty_compact(), &[]);
        assert!(html.contains("\u{201c}testing new feature X\u{201d}"));
    }

    #[test]
    fn run_tables_multi_family_with_delta() {
        let html = render_run_tables(&versus_compact());
        // Three tables.
        assert!(html.contains("<h2>QPS at recall</h2>"));
        assert!(html.contains("<h2>Latency at recall</h2>"));
        assert!(html.contains("<h2>Build comparison</h2>"));
        // Δ column against the best competitor.
        assert!(html.contains("\u{394} vs best competitor"));
        // Both frontiers hit 0.90 exactly: measured values, comparable Δ.
        assert!(html.contains("3,000"));
        assert!(html.contains("<b>2,000</b>"));
        assert!(html.contains("class=\"pos\">+50%"));
        // At 0.95 the competitor's sweep tops out at 0.94; no Δ there.
        assert!(html.contains("(max r=0.940)"));
        // 0.99 spanned by nobody -> no such row in either table.
        assert!(!html.contains("0.99"));
        // Caption: method + basis + workload note; footnotes name configs.
        assert!(html.contains("log-interpolated"));
        assert!(html.contains("distance-based"));
        assert!(html.contains("Insert/update/delete workloads are not measured"));
        assert!(html.contains("Interpolated on config"));
        assert!(html.contains("never interpolated across different configurations"));
        assert!(html.contains("PRISM = prism-n16000, prewarmed"));
        // Build cells with prism faster + smaller: green negative deltas.
        assert!(html.contains("class=\"pos\">-39%"));
    }

    #[test]
    fn run_tables_delta_only_between_interpolated_cells() {
        // PRISM's sweep starts above 0.90 — no value there, so no Δ,
        // even though the competitor hits 0.90 exactly.
        let html = render_run_tables(&compact(
            vec![
                sweep("prism", "prism", 0.93, 3000.0),
                sweep("prism", "prism", 0.96, 1500.0),
                sweep("hnsw", "pgvector-hnsw", 0.90, 2000.0),
                sweep("hnsw", "pgvector-hnsw", 0.96, 800.0),
            ],
            vec![],
        ));
        assert!(html.contains("(min r=0.930)")); // PRISM's 0.90 cell
        let qps_table = &html[..html.find("Latency at recall").unwrap()];
        let row_090 = &qps_table[qps_table.find("<td>0.90</td>").unwrap()..];
        let row_090 = &row_090[..row_090.find("</tr>").unwrap()];
        assert!(row_090.contains("—")); // no Δ at 0.90
        assert!(!row_090.contains("class=\"pos\""));
        // At 0.95 both interpolate -> Δ present.
        let row_095 = &qps_table[qps_table.find("<td>0.95</td>").unwrap()..];
        let row_095 = &row_095[..row_095.find("</tr>").unwrap()];
        assert!(row_095.contains("class=\"pos\"") || row_095.contains("class=\"neg\""));
    }

    #[test]
    fn run_tables_single_family_without_delta() {
        let html = render_run_tables(&compact(
            vec![
                sweep("prism-serial", "prism", 0.90, 2000.0),
                sweep("prism-parallel", "prism", 0.90, 2100.0),
            ],
            vec![],
        ));
        // Columns are the config names, no Δ column.
        assert!(html.contains("<th>prism-serial</th>"));
        assert!(html.contains("<th>prism-parallel</th>"));
        assert!(!html.contains("\u{394} vs best competitor"));
        // Best value per row still bolded.
        assert!(html.contains("<b>2,100</b>"));
        // No builds recorded -> no build table.
        assert!(!html.contains("Build comparison"));
    }

    #[test]
    fn run_tables_show_throughput_under_load_after_qps_at_recall() {
        use crate::publish::summary::test_load_point as load;
        let mut cs = versus_compact();
        cs.load_points = vec![
            load(
                "prism-n16000",
                "prism",
                &[0.90],
                0.90,
                1,
                None,
                3000.0,
                3000.0,
            ),
            load(
                "prism-n16000",
                "prism",
                &[0.90],
                0.90,
                8,
                Some("1x"),
                12000.0,
                12500.0,
            ),
            load(
                "hnsw-m24",
                "pgvector-hnsw",
                &[0.90],
                0.90,
                8,
                Some("1x"),
                8000.0,
                8400.0,
            ),
        ];
        let html = render_run_tables(&cs);

        let qps = html.find("<h2>QPS at recall</h2>").unwrap();
        let load_at = html.find("<h2>Throughput under load</h2>").unwrap();
        let latency = html.find("<h2>Latency at recall</h2>").unwrap();
        assert!(qps < load_at && load_at < latency);
        let table = &html[load_at..latency];

        assert!(table.contains("<td>0.90 · 1 client</td>"));
        assert!(table.contains("0.90 · 8 clients <span class=\"faint\">(1x of 8 CPUs)</span>"));
        // Wall-clock QPS, then the all-clients-busy throughput in parentheses.
        // Latency below: p50 = 8 clients / 12,500 QPS = 0.64 ms, p95 twice that.
        // Speedup over PRISM's own single client: 12,000 / 3,000 = 4.0x.
        assert!(table.contains(
            "<td>12,000 <span class=\"faint\">(12,500)</span> · 4.0\u{d7}<br>\
             p50 0.64 ms · p95 1.28 ms</td>"
        ));
        // The best competitor is bolded, and PRISM leads it by 12000/8000.
        // HNSW has no single-client baseline here, so no speedup.
        assert!(table.contains(
            "<td><b>8,000</b> <span class=\"faint\">(8,400)</span><br>p50 0.95 ms · p95 1.90 ms</td>"
        ));
        // The baseline row itself carries no speedup.
        assert!(table.contains(
            "<td>3,000 <span class=\"faint\">(3,000)</span><br>p50 0.33 ms · p95 0.67 ms</td>"
        ));
        assert!(table.contains("is the speedup"));
        assert!(table.contains("class=\"pos\">+50%"));
        // No competitor baseline, so no delta on the single-client row.
        let baseline_row = &table[table.find("1 client</td>").unwrap()..];
        let baseline_row = &baseline_row[..baseline_row.find("</tr>").unwrap()];
        assert!(!baseline_row.contains("class=\"pos\""));
        assert!(table.contains("not interpolated"));
        // One footnote per family and target, since a single config served
        // every client count.
        assert!(table.contains("PRISM @0.90: prism-n16000 (r=0.900)"));
        assert!(table.contains("pgvector HNSW @0.90: hnsw-m24 (r=0.900)"));
        assert_eq!(table.matches("prism-n16000 (r=").count(), 1);
    }

    #[test]
    fn load_table_footnote_names_client_counts_when_configs_differ() {
        use crate::publish::summary::test_load_point as load;
        let mut cs = versus_compact();
        cs.load_points = vec![
            load(
                "prism-n16000",
                "prism",
                &[0.90],
                0.90,
                8,
                Some("1x"),
                12000.0,
                12500.0,
            ),
            load(
                "prism-other",
                "prism",
                &[0.90],
                0.91,
                16,
                Some("2x"),
                14000.0,
                15000.0,
            ),
        ];
        let html = render_run_tables(&cs);
        assert!(html.contains("PRISM @0.90 with 8 clients: prism-n16000 (r=0.900)"));
        assert!(html.contains("PRISM @0.90 with 16 clients: prism-other (r=0.910)"));
    }

    #[test]
    fn run_tables_without_load_points_have_no_load_table() {
        assert!(!render_run_tables(&versus_compact()).contains("Throughput under load"));
    }

    #[test]
    fn run_tables_empty_run_renders_nothing() {
        assert_eq!(render_run_tables(&empty_compact()), "");
    }

    #[test]
    fn fmt_thousands_groups_digits() {
        assert_eq!(fmt_thousands(999.0), "999");
        assert_eq!(fmt_thousands(1234.0), "1,234");
        assert_eq!(fmt_thousands(1234567.0), "1,234,567");
    }

    // Writes a sample run page + catalog with realistic multi-family data to
    // the temp dir for eyeballing layout changes; prints the paths. Run with
    // `cargo test -p rekall-lib sample_pages -- --ignored --nocapture`.
    #[test]
    #[ignore = "writes sample HTML for visual inspection; run on demand"]
    fn sample_pages_to_tmp() {
        let mut m = meta("cohere-5m", "2026-06-10", "09:56:42");
        m.comment = Some("sample page for layout review".into());
        let run = std::env::temp_dir().join("rekall-sample-run-page.html");
        let mut cs = versus_compact();
        cs.load_points = {
            use crate::publish::summary::test_load_point as load;
            let (prism_arm, hnsw) = ("prism-n16000", "hnsw-m24");
            vec![
                load(prism_arm, "prism", &[0.90], 0.90, 1, None, 3000.0, 3000.0),
                load(
                    prism_arm,
                    "prism",
                    &[0.90],
                    0.90,
                    48,
                    Some("1x"),
                    52000.0,
                    54500.0,
                ),
                load(
                    prism_arm,
                    "prism",
                    &[0.90],
                    0.90,
                    96,
                    Some("2x"),
                    61000.0,
                    66000.0,
                ),
                load(
                    hnsw,
                    "pgvector-hnsw",
                    &[0.90],
                    0.90,
                    1,
                    None,
                    2000.0,
                    2000.0,
                ),
                load(
                    hnsw,
                    "pgvector-hnsw",
                    &[0.90],
                    0.90,
                    48,
                    Some("1x"),
                    36000.0,
                    37800.0,
                ),
                load(
                    hnsw,
                    "pgvector-hnsw",
                    &[0.90],
                    0.90,
                    96,
                    Some("2x"),
                    40000.0,
                    44000.0,
                ),
                load(prism_arm, "prism", &[0.95], 0.96, 1, None, 1500.0, 1500.0),
                load(
                    prism_arm,
                    "prism",
                    &[0.95],
                    0.96,
                    48,
                    Some("1x"),
                    27000.0,
                    28100.0,
                ),
                load(
                    prism_arm,
                    "prism",
                    &[0.95],
                    0.96,
                    96,
                    Some("2x"),
                    31000.0,
                    33500.0,
                ),
            ]
            .into_iter()
            .map(|mut p| {
                p.cpu_count = 48;
                p
            })
            .collect()
        };
        std::fs::write(&run, render_run_page(&summary_for(&m), &m, &cs, &[])).unwrap();
        let mut with = entry(m);
        with.summary = Some(versus_compact());
        let old = entry(meta("cohere-5m", "2026-06-09", "08:00:00"));
        let catalog = std::env::temp_dir().join("rekall-sample-catalog.html");
        std::fs::write(&catalog, render_catalog(&[with, old])).unwrap();
        println!("run page: {}", run.display());
        println!("catalog:  {}", catalog.display());
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512.0 B");
        assert_eq!(human_bytes(2 * 1024 * 1024), "2.0 MB");
    }

    fn summary_for(meta: &RunMeta) -> RunSummary {
        use crate::metrics::{HostInfo, RekallInfo, RunEnvironment};
        RunSummary {
            dataset: meta.dataset.clone(),
            environment: Some(RunEnvironment {
                host: HostInfo {
                    hostname: meta.hostname.clone(),
                    os: "Ubuntu 24.04".into(),
                    kernel: "6.17".into(),
                    cpu_model: meta.cpu_model.clone(),
                    cpu_count: meta.cpu_count,
                    memory_gb: meta.memory_gb,
                    ip_address: None,
                    instance_type: meta.instance_type.clone(),
                    storage: None,
                },
                rekall: RekallInfo {
                    version: "0.1.0".into(),
                    git_commit: meta.rekall_commit.clone(),
                    git_branch: Some("main".into()),
                    note: String::new(),
                },
                timestamp: meta.timestamp.clone(),
                comment: meta.comment.clone(),
            }),
            config: serde_json::json!({"search": {"k": 10, "runs": 3, "num_queries": 10000}}),
            pg_runtime: None,
            index_builds: Vec::new(),
            search_results: Vec::new(),
            concurrency_results: Vec::new(),
            pareto: Vec::new(),
        }
    }

    #[test]
    fn run_page_shows_metadata_and_only_present_plot_links() {
        let m = meta("cohere-5m", "2026-06-10", "09:56:42");
        // Only pareto + per-config were generated for this run.
        let files = [
            PathBuf::from("cohere-5m-report-pareto.html"),
            PathBuf::from("cohere-5m-report-pareto.svg"),
            PathBuf::from("cohere-5m-report-per-config.html"),
        ];
        let html = render_run_page(&summary_for(&m), &m, &empty_compact(), &files);
        assert!(html.contains("Cohere-5M"));
        assert!(html.contains("r8i.2xlarge")); // instance
        assert!(html.contains("Xeon")); // cpu
        assert!(html.contains("num_queries")); // search config
        assert!(html.contains("cohere-5m-report-pareto.html")); // interactive link
        assert!(html.contains("cohere-5m-report-pareto.svg")); // svg download / thumb
        assert!(html.contains("class=\"svg\"")); // svg download link present
        assert!(html.contains("s3://rekall/runs/x/")); // s3 link
                                                       // distance-recall wasn't generated → must NOT be linked (no 404s).
        assert!(!html.contains("distance-recall"));
    }

    // Full git flow against a local bare "origin": clone, create gh-pages,
    // write the run page + catalog, commit, push. Ignored by default (needs
    // git and writes temp dirs); run with `cargo test -p rekall-lib pages -- --ignored`.
    #[test]
    #[ignore = "exercises real git; run on demand"]
    fn end_to_end_push_to_local_origin() {
        let base = std::env::temp_dir().join(format!("rekall-pages-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let origin = base.join("origin.git");
        Command::new("git")
            .args(["init", "--bare", "-b", "main", origin.to_str().unwrap()])
            .status()
            .unwrap();
        let repo = base.join("repo");
        Command::new("git")
            .args(["clone", origin.to_str().unwrap(), repo.to_str().unwrap()])
            .status()
            .unwrap();

        let results = base.join("results");
        std::fs::create_dir_all(&results).unwrap();

        // Isolate the pages workdir under the test base.
        unsafe { std::env::set_var("REKALL_PAGES_WORKDIR", base.join("work")) };

        let m = meta("cohere-5m", "2026-06-10", "09:56:42");
        let cfg = PublishConfig {
            repo_path: Some(repo.to_string_lossy().into_owned()),
            public_base_url: Some("https://x.github.io/rekall".into()),
            ..Default::default()
        };
        let url = publish(
            &summary_for(&m),
            &m,
            &versus_compact(),
            &results,
            &m.run_id,
            &cfg,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            url.as_deref(),
            Some("https://x.github.io/rekall/runs/cohere-5m/2026-06-10/i-095642/")
        );

        // Verify by checking out gh-pages from origin.
        let verify = base.join("verify");
        Command::new("git")
            .args([
                "clone",
                "--branch",
                "gh-pages",
                origin.to_str().unwrap(),
                verify.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(verify.join("index.html").exists());
        assert!(verify.join(".nojekyll").exists());
        assert!(verify
            .join("runs/cohere-5m/2026-06-10/i-095642/index.html")
            .exists());
        assert!(verify
            .join("runs/cohere-5m/2026-06-10/i-095642/meta.json")
            .exists());
        // summary.json feeds the catalog's comparison column.
        assert!(verify
            .join("runs/cohere-5m/2026-06-10/i-095642/summary.json")
            .exists());
        let catalog = std::fs::read_to_string(verify.join("index.html")).unwrap();
        assert!(catalog.contains("vs pgvector HNSW:"));

        // Commit count on the bare origin's gh-pages branch.
        let count = |bare: &Path| -> usize {
            let out = Command::new("git")
                .args([
                    "-C",
                    bare.to_str().unwrap(),
                    "rev-list",
                    "--count",
                    "gh-pages",
                ])
                .output()
                .unwrap();
            String::from_utf8(out.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        let n1 = count(&origin);

        // Re-publishing identical content WITHOUT --force is a no-op (the
        // rendered pages are deterministic, so nothing changes).
        publish(
            &summary_for(&m),
            &m,
            &versus_compact(),
            &results,
            &m.run_id,
            &cfg,
            false,
            false,
        )
        .unwrap();
        assert_eq!(count(&origin), n1, "unchanged re-publish must not commit");

        // WITH --force it republishes even when byte-identical (empty commit).
        publish(
            &summary_for(&m),
            &m,
            &versus_compact(),
            &results,
            &m.run_id,
            &cfg,
            false,
            true,
        )
        .unwrap();
        assert_eq!(
            count(&origin),
            n1 + 1,
            "--force must republish regenerated artifacts even when unchanged"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
