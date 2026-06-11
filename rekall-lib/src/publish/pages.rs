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

use super::report;
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

/// Stage, commit, and push the branch. No-op (Ok) when nothing changed.
fn commit_push(work: &Path, branch: &str, ssh: Option<&str>, msg: &str) -> Result<()> {
    git(work, None, &["add", "-A"])?;
    if git(work, None, &["status", "--porcelain"])?.is_empty() {
        tracing::info!("gh-pages: nothing changed, skipping commit");
        return Ok(());
    }
    git(
        work,
        None,
        &[
            "-c",
            "user.email=rekall@localhost",
            "-c",
            "user.name=rekall",
            "commit",
            "-m",
            msg,
        ],
    )?;
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
pub fn publish(
    summary: &RunSummary,
    meta: &RunMeta,
    results_dir: &Path,
    run_id: &str,
    config: &PublishConfig,
    dry_run: bool,
) -> Result<Option<String>> {
    let report_files = collect_report_files(summary, results_dir);

    if dry_run {
        println!("[dry-run] gh-pages would update runs/{run_id}/:");
        println!("  runs/{run_id}/index.html  (run metadata + plots)");
        println!("  runs/{run_id}/meta.json");
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
    std::fs::write(
        run_dir.join("index.html"),
        render_run_page(summary, meta, &report_files),
    )
    .map_err(|e| Error::Config(format!("write run index.html: {e}")))?;

    // Regenerate the catalog from every run's meta.json.
    let metas = load_all_meta(&work);
    std::fs::write(work.join("index.html"), render_catalog(&metas))
        .map_err(|e| Error::Config(format!("write catalog index.html: {e}")))?;

    commit_push(&work, branch, ssh.as_deref(), &format!("publish {run_id}"))?;
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
    for kind in ["pareto", "per-config", "distance-recall", "build-times"] {
        for ext in ["html", "svg"] {
            let p = dir.join(format!("{prefix}-report-{kind}.{ext}"));
            if p.exists() {
                out.push(p);
            }
        }
    }
    out
}

fn load_all_meta(work: &Path) -> Vec<RunMeta> {
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
                        out.push(m);
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
.kv td:first-child{color:#555;width:230px}\
details{margin:8px 0}summary{cursor:pointer;color:#555;font-size:13px}\
pre{background:#f6f8fa;padding:10px;border-radius:6px;overflow:auto;font-size:12px}\
.thumbs img{max-width:520px;border:1px solid #eee;border-radius:6px;margin:8px 8px 0 0}";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{}</title>\
         <style>{STYLE}</style></head><body><div class=\"wrap\">{body}</div></body></html>",
        esc(title)
    )
}

/// Full per-run metadata page.
fn render_run_page(summary: &RunSummary, meta: &RunMeta, report_files: &[PathBuf]) -> String {
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

    // Plots — link only the report files that were actually generated for this
    // run (distance-recall, e.g., only exists if a ground-truth recompute ran).
    b.push_str("<h2>Plots</h2>");
    let prefix = report::report_prefix(summary);
    let present: std::collections::HashSet<String> = report_files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
        .collect();
    let has = |kind: &str, ext: &str| present.contains(&format!("{prefix}-report-{kind}.{ext}"));

    let links: Vec<String> = [
        ("pareto", "Pareto"),
        ("per-config", "Per-config"),
        ("distance-recall", "Distance-recall"),
        ("build-times", "Build times"),
    ]
    .iter()
    .filter(|(kind, _)| has(kind, "html"))
    .map(|(kind, label)| {
        format!(
            "<a href=\"{}\">{label}</a>",
            esc(&format!("{prefix}-report-{kind}.html"))
        )
    })
    .collect();

    if links.is_empty() {
        b.push_str("<p class=\"sub\">(no plots for this run)</p>");
    } else {
        b.push_str(&links.join(" &nbsp;·&nbsp; "));
        b.push_str("<div class=\"thumbs\">");
        for kind in ["pareto", "per-config", "build-times"] {
            if has(kind, "svg") {
                let svg = format!("{prefix}-report-{kind}.svg");
                b.push_str(&format!(
                    "<a href=\"{svg}\"><img src=\"{svg}\" alt=\"{kind}\"></a>",
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
    b.push_str("</table>");

    // Search config
    b.push_str("<h2>Search</h2><table class=\"kv\">");
    let sc = &summary.config["search"];
    for k in ["k", "runs", "warmup", "num_queries"] {
        if let Some(v) = sc.get(k) {
            row(&mut b, k, &v.to_string());
        }
    }
    row(&mut b, "S3 archive", &meta.s3_url);
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

/// Top-level catalog grouped by dataset, newest run first.
fn render_catalog(metas: &[RunMeta]) -> String {
    use std::collections::BTreeMap;
    let mut by_ds: BTreeMap<String, Vec<&RunMeta>> = BTreeMap::new();
    for m in metas {
        by_ds.entry(m.dataset.clone()).or_default().push(m);
    }

    let mut b = String::new();
    b.push_str("<h1>Rekall benchmark runs</h1>");
    b.push_str(&format!(
        "<div class=\"sub\">{} runs across {} datasets</div>",
        metas.len(),
        by_ds.len()
    ));

    for (dataset, mut runs) in by_ds {
        // newest first by (date, time)
        runs.sort_by(|a, b| (&b.date, &b.time).cmp(&(&a.date, &a.time)));
        b.push_str(&format!("<h2>{}</h2>", esc(&pretty_dataset(&dataset))));
        b.push_str(
            "<table><tr><th>Date</th><th>Instance</th><th>Peak recall@QPS</th>\
             <th>Commit</th><th>Plots</th><th>Data</th></tr>",
        );
        for m in runs {
            let run_path = format!("runs/{}", m.run_id);
            let prefix = if m.dataset.is_empty() {
                "report".to_string()
            } else {
                m.dataset.clone()
            };
            let inst = m.instance_type.as_deref().unwrap_or(&m.hostname);
            let commit = m.rekall_commit.as_deref().unwrap_or("—");
            b.push_str(&format!(
                "<tr><td><a href=\"{rp}/index.html\">{date} · {time}</a></td>\
                 <td>{inst}</td><td>{recall:.4} @ {qps:.0}</td><td>{commit}</td>\
                 <td><a href=\"{rp}/{pre}-report-pareto.html\">pareto</a> · \
                 <a href=\"{rp}/{pre}-report-per-config.html\">per-config</a></td>\
                 <td><a href=\"{s3}\">S3</a></td></tr>",
                rp = esc(&run_path),
                date = esc(&m.date),
                time = esc(&m.time),
                inst = esc(inst),
                recall = m.peak_recall,
                qps = m.qps_at_peak,
                commit = esc(commit),
                pre = esc(&prefix),
                s3 = esc(&m.s3_url),
            ));
        }
        b.push_str("</table>");
    }
    page("Rekall benchmark runs", &b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
        let metas = vec![
            meta("cohere-5m", "2026-06-10", "09:56:42"),
            meta("cohere-5m", "2026-06-11", "10:00:00"),
            meta("cohere-10m", "2026-06-09", "08:00:00"),
        ];
        let html = render_catalog(&metas);
        assert!(html.contains("<h2>Cohere-5M</h2>"));
        assert!(html.contains("<h2>Cohere-10M</h2>"));
        // newest cohere-5m run (06-11) appears before the older (06-10)
        let p11 = html.find("2026-06-11").unwrap();
        let p10 = html.find("2026-06-10").unwrap();
        assert!(p11 < p10);
        // deep link to a run page + plot
        assert!(html.contains("runs/cohere-5m/2026-06-10/i-095642/index.html"));
        assert!(html.contains("cohere-5m-report-pareto.html"));
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
                },
                rekall: RekallInfo {
                    version: "0.1.0".into(),
                    git_commit: meta.rekall_commit.clone(),
                    git_branch: Some("main".into()),
                    note: String::new(),
                },
                timestamp: meta.timestamp.clone(),
            }),
            config: serde_json::json!({"search": {"k": 10, "runs": 3, "num_queries": 10000}}),
            pg_runtime: None,
            index_builds: Vec::new(),
            search_results: Vec::new(),
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
        let html = render_run_page(&summary_for(&m), &m, &files);
        assert!(html.contains("Cohere-5M"));
        assert!(html.contains("r8i.2xlarge")); // instance
        assert!(html.contains("Xeon")); // cpu
        assert!(html.contains("num_queries")); // search config
        assert!(html.contains("cohere-5m-report-pareto.html")); // present plot link
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
        let url = publish(&summary_for(&m), &m, &results, &m.run_id, &cfg, false).unwrap();
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

        let _ = std::fs::remove_dir_all(&base);
    }
}
