//! Publishing: archive a run's results directory to S3 and push HTML reports
//! + a catalog to GitHub Pages.
//!
//! The run-id is **deterministic from the run's recorded metadata** (dataset,
//! run-start date/time, instance), so re-publishing a run maps to the same
//! S3 prefix / Pages entry rather than creating a duplicate.

mod compare;
mod distance;
mod pages;
pub mod report;
mod s3;
pub mod summary;

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::PublishConfig;
use crate::error::{Error, Result};
use crate::metrics::{IndexBuildResult, RunSummary};

/// Knobs controlling a single publish invocation (CLI flags map onto these).
#[derive(Debug, Default, Clone)]
pub struct PublishOptions {
    /// Overwrite even if the run was already uploaded.
    pub force: bool,
    /// Print what would be uploaded/pushed without writing anything.
    pub dry_run: bool,
    /// Archive to S3 only; skip GitHub Pages.
    pub s3_only: bool,
    /// Skip GitHub Pages (alias of s3_only intent from config side).
    pub no_pages: bool,
    /// Override the configured S3 bucket.
    pub bucket: Option<String>,
    /// Override the configured/region-chain region.
    pub region: Option<String>,
    /// Dataset HDF5 for the distance-recall plot. When unset, the run's
    /// `ground-truth.json.gz` (or its configured dataset) is used.
    pub dataset: Option<std::path::PathBuf>,
}

/// Compact per-run metadata: the S3 completion marker and the source for the
/// Pages catalog (so the index can be rebuilt without the full RunSummary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub run_id: String,
    pub dataset: String,
    pub date: String,
    pub time: String,
    pub timestamp: String,
    pub instance_type: Option<String>,
    pub hostname: String,
    pub cpu_model: String,
    pub cpu_count: usize,
    pub memory_gb: f64,
    pub pg_version: Option<String>,
    pub rekall_commit: Option<String>,
    /// extension name -> "version (commit)".
    pub extensions: BTreeMap<String, String>,
    pub peak_recall: f64,
    pub qps_at_peak: f64,
    pub num_indexes: usize,
    pub s3_url: String,
    /// Free-form run description (`rekall run --comment`), shown in the
    /// catalog and on the per-run page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl RunMeta {
    fn from_summary(s: &RunSummary, run_id: &str, s3_url: &str) -> Result<Self> {
        let env = s.environment.as_ref().ok_or_else(|| {
            Error::Config("results have no recorded environment metadata".to_string())
        })?;
        let (date, time) = split_timestamp(&env.timestamp)?;

        // Peak = the highest-recall sweep point and the QPS achieved there.
        let (peak_recall, qps_at_peak) =
            s.search_results
                .iter()
                .fold((0.0_f64, 0.0_f64), |(br, bq), r| {
                    if r.avg_recall > br {
                        (r.avg_recall, r.qps)
                    } else {
                        (br, bq)
                    }
                });

        let extensions = s
            .pg_runtime
            .as_ref()
            .map(|pg| {
                pg.extensions
                    .iter()
                    .map(|(k, v)| {
                        let tag = match &v.git_commit {
                            Some(c) => {
                                let short: String = c.chars().take(7).collect();
                                format!("{} ({short})", v.version)
                            }
                            None => v.version.clone(),
                        };
                        (k.clone(), tag)
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            run_id: run_id.to_string(),
            dataset: s.dataset.clone(),
            date,
            time,
            timestamp: env.timestamp.clone(),
            instance_type: env.host.instance_type.clone(),
            hostname: env.host.hostname.clone(),
            cpu_model: env.host.cpu_model.clone(),
            cpu_count: env.host.cpu_count,
            memory_gb: env.host.memory_gb,
            pg_version: s.pg_runtime.as_ref().map(|p| p.version.clone()),
            rekall_commit: env.rekall.git_commit.clone(),
            extensions,
            peak_recall,
            qps_at_peak,
            num_indexes: s.index_builds.len(),
            s3_url: s3_url.to_string(),
            comment: env.comment.clone(),
        })
    }
}

/// Split an ISO timestamp like `2026-06-10T09:56:42+0000` into
/// (`2026-06-10`, `09:56:42`).
fn split_timestamp(ts: &str) -> Result<(String, String)> {
    let (date, rest) = ts
        .split_once('T')
        .ok_or_else(|| Error::Config(format!("unexpected timestamp format: {ts}")))?;
    // Drop the timezone suffix (+hhmm / -hhmm / Z); the time itself has no '-'.
    let time = rest.split(['+', '-', 'Z']).next().unwrap_or(rest);
    Ok((date.to_string(), time.to_string()))
}

/// Keep path/key-safe characters; replace anything else with '-'.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Deterministic run-id: `<dataset>/<YYYY-MM-DD>/<instance-or-host>-<HHMMSS>`.
pub fn run_id(s: &RunSummary) -> Result<String> {
    let env = s.environment.as_ref().ok_or_else(|| {
        Error::Config(
            "cannot derive run-id: results have no recorded environment/timestamp \
             (need results.json or run-info-*.json)"
                .to_string(),
        )
    })?;
    let (date, time) = split_timestamp(&env.timestamp)?;
    let hhmmss: String = time
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(6)
        .collect();
    let instance = env
        .host
        .instance_type
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env.host.hostname.clone());
    let dataset = if s.dataset.is_empty() {
        "unknown".to_string()
    } else {
        s.dataset.clone()
    };
    Ok(format!(
        "{}/{}/{}-{}",
        sanitize(&dataset),
        sanitize(&date),
        sanitize(&instance),
        hhmmss
    ))
}

/// Read a `RunSummary` from a `.json` or `.json.gz` file.
fn read_summary_file(path: &Path) -> Result<RunSummary> {
    use std::io::Read as _;
    let is_gz = path.extension().and_then(|e| e.to_str()) == Some("gz");
    let content = if is_gz {
        let f = std::fs::File::open(path)
            .map_err(|e| Error::Config(format!("read {}: {e}", path.display())))?;
        let mut d = flate2::read::GzDecoder::new(f);
        let mut s = String::new();
        d.read_to_string(&mut s)
            .map_err(|e| Error::Config(format!("decompress {}: {e}", path.display())))?;
        s
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("read {}: {e}", path.display())))?
    };
    serde_json::from_str(&content)
        .map_err(|e| Error::Config(format!("invalid result JSON {}: {e}", path.display())))
}

/// Load the aggregated `RunSummary` for a results dir by merging the per-index
/// `*.json.gz` files (the source of truth). Older runs also wrote a
/// consolidated `results.json`; when present it is used as the base for
/// backwards compatibility, but it is no longer produced at benchmark time.
///
/// In both cases the per-index files' `pg_runtime.extensions` are unioned into
/// the result. A legacy `results.json` captured `pg_runtime` once, early in the
/// run — often before `CREATE EXTENSION meerkat` runs in the index phase — so
/// it can omit meerkat's version/commit entirely. The per-index files (written
/// after each build) do record it, so merging them in restores the full
/// extension set for the report's Versions table and plot captions.
pub fn load_run_summary(dir: &Path) -> Result<RunSummary> {
    let mut gz: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| Error::Config(format!("scan {}: {e}", dir.display())))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.ends_with(".json.gz") && !name.starts_with("ground-truth")
        })
        .collect();
    gz.sort();

    // Base summary: the consolidated results.json if present, else the merge of
    // the per-index files.
    let consolidated = ["results.json", "results.json.gz"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.exists());

    let mut base = if let Some(p) = consolidated {
        read_summary_file(&p)?
    } else {
        if gz.is_empty() {
            return Err(Error::Config(format!(
                "no result files (results.json or per-index *.json.gz) in {}",
                dir.display()
            )));
        }
        let mut b = read_summary_file(&gz[0])?;
        for p in &gz[1..] {
            let s = read_summary_file(p)?;
            b.index_builds.extend(s.index_builds);
            b.search_results.extend(s.search_results);
        }
        b
    };

    // Always union per-index extensions (with their git commits) into the base,
    // so a meerkat commit recorded per-build is never lost — even when the base
    // came from a results.json whose early pg_runtime capture predates the
    // extension's creation. Concurrency results only ever live in per-index
    // files, so they are gathered here too, whichever base was used.
    let mut concurrency_files = Vec::new();
    for p in &gz {
        if let Ok(s) = read_summary_file(p) {
            merge_pg_runtime(&mut base.pg_runtime, s.pg_runtime);
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            concurrency_files.push((name, s.concurrency_results));
        }
    }
    base.concurrency_results = crate::metrics::latest_concurrency_results(concurrency_files);

    // Surface a run comment (`rekall run --comment`) recorded by ANY arm:
    // the base summary comes from the alphabetically-first file, which is
    // not necessarily the session that carried the comment (incremental
    // arms added to an existing results dir record theirs in their own
    // file). First non-empty comment in filename order wins.
    let base_has_comment = base
        .environment
        .as_ref()
        .is_some_and(|e| e.comment.as_deref().is_some_and(|c| !c.is_empty()));
    if !base_has_comment {
        for p in &gz {
            let Ok(s) = read_summary_file(p) else {
                continue;
            };
            let Some(env) = s.environment else { continue };
            if env.comment.as_deref().is_some_and(|c| !c.is_empty()) {
                match base.environment.as_mut() {
                    Some(base_env) => base_env.comment = env.comment,
                    None => base.environment = Some(env),
                }
                break;
            }
        }
    }

    // Build info: the separate build-<name>.json files are authoritative (they
    // survive search-only re-runs, where the query files carry no index_builds).
    // Cross-check against the build info embedded in the query files and warn on
    // any divergence; the embedded copy is only a fallback when no build file
    // exists for an index.
    base.index_builds = resolve_build_results(dir, &base.index_builds);
    Ok(base)
}

/// Resolve per-index build results for a results dir.
///
/// The separate `build-<name>.json` files are the source of truth: they are
/// written once at build time and persist across search-only re-runs (which
/// produce result files with empty `index_builds`). The build info embedded in
/// query result files is used only as a cross-check — if it disagrees with the
/// build file we warn and keep the build file's value; if a query file carries
/// build info for an index that has no build file, we fall back to it so older
/// runs still report.
pub fn resolve_build_results(dir: &Path, embedded: &[IndexBuildResult]) -> Vec<IndexBuildResult> {
    // 1. Authoritative: build-<name>.json files in the dir.
    let mut by_name: BTreeMap<String, IndexBuildResult> = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("build-") && name.ends_with(".json") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(br) = serde_json::from_str::<IndexBuildResult>(&content) {
                        by_name.insert(br.index_name.clone(), br);
                    }
                }
            }
        }
    }

    // 2. Cross-check against / fall back to the build info embedded in query files.
    for q in embedded {
        match by_name.get(&q.index_name) {
            Some(file) => {
                if file.build_time != q.build_time || file.index_size_bytes != q.index_size_bytes {
                    tracing::warn!(
                        "build info for '{}' differs between build-{}.json \
                         ({:.0}s, {} bytes) and the query result file \
                         ({:.0}s, {} bytes); using the build file",
                        q.index_name,
                        q.index_name,
                        file.build_time.as_secs_f64(),
                        file.index_size_bytes,
                        q.build_time.as_secs_f64(),
                        q.index_size_bytes,
                    );
                }
            }
            None => {
                by_name.insert(q.index_name.clone(), q.clone());
            }
        }
    }

    by_name.into_values().collect()
}

/// Merge one per-index file's `pg_runtime` into the accumulator. Different
/// index types load different extensions (an hnsw run records pgvector but not
/// meerkat, and vice-versa), so the union across files is what reflects the
/// whole run. Adopt `pg_runtime` if absent; otherwise union the extensions,
/// preferring an entry that carries a git commit over one that doesn't.
///
/// Public so `rekall report` reconciles versions across per-index files the
/// same way `publish` does.
pub fn merge_pg_runtime(
    base: &mut Option<crate::metrics::PgRuntimeInfo>,
    other: Option<crate::metrics::PgRuntimeInfo>,
) {
    let Some(other) = other else { return };
    match base {
        None => *base = Some(other),
        Some(base) => {
            for (name, info) in other.extensions {
                let replace = match base.extensions.get(&name) {
                    None => true,
                    Some(existing) => existing.git_commit.is_none() && info.git_commit.is_some(),
                };
                if replace {
                    base.extensions.insert(name, info);
                }
            }
        }
    }
}

/// Archive `dir` to S3 (and, in Stage 2, publish to GitHub Pages).
pub async fn publish(dir: &Path, config: &PublishConfig, opts: &PublishOptions) -> Result<()> {
    let summary = load_run_summary(dir)?;
    let rid = run_id(&summary)?;

    let bucket = opts
        .bucket
        .clone()
        .unwrap_or_else(|| config.bucket().to_string());
    let region = opts.region.clone().or_else(|| config.s3_region.clone());
    let prefix = format!("{}/{}", config.s3_prefix.trim_matches('/'), rid);
    let s3_url = format!("s3://{bucket}/{prefix}/");
    let marker_key = format!("{prefix}/meta.json");
    let meta = RunMeta::from_summary(&summary, &rid, &s3_url)?;
    let do_pages = config.pages && !opts.s3_only && !opts.no_pages;

    // Final recall values, computed once and shared by report generation,
    // summary.json, and the run-page tables — one basis for the whole run.
    // Dry-run skips the ground-truth load (it can pull in a multi-GB HDF5).
    let finals = if opts.dry_run {
        summary::FinalResults::id_basis(&summary)
    } else {
        summary::compute_final_results(&summary, dir, opts.dataset.as_deref())
    };
    let compact = summary::CompactSummary::build(&summary, &finals);

    // Step 1: generate the report HTML/SVG + summary.json into the results
    // dir so they are archived to S3 *and* available for the Pages copy.
    // (Skipped on dry-run.)
    if !opts.dry_run {
        report::generate_into_dir(&summary, dir, &finals);
        summary::write_summary_json(dir, &compact)?;
    }

    let mut files =
        s3::walk_files(dir).map_err(|e| Error::Config(format!("scan {}: {e}", dir.display())))?;
    files.retain(|p| !is_excluded(p));

    if opts.dry_run {
        println!("[dry-run] would archive {} files to {s3_url}", files.len());
        for f in &files {
            let rel = rel_key(dir, f);
            if rel == "meta.json" {
                continue;
            }
            println!("  {s3_url}{rel}");
        }
        println!("  {s3_url}summary.json  (compact table/catalog data)");
        println!("  {s3_url}meta.json  (completion marker, written last)");
        if do_pages {
            pages::publish(
                &summary, &meta, &compact, dir, &rid, config, true, opts.force,
            )?;
        }
        return Ok(());
    }

    let mut up = s3::Uploader::new(region.as_deref(), &bucket).await?;
    up.ensure_bucket().await?;

    if up.object_exists(&marker_key).await? && !opts.force {
        tracing::warn!("run already uploaded at {s3_url} — skipping (use --force to overwrite)");
        return Ok(());
    }

    for f in &files {
        let rel = rel_key(dir, f);
        // The canonical meta.json is written last as the completion marker.
        if rel == "meta.json" {
            continue;
        }
        up.put_file(&format!("{prefix}/{rel}"), f).await?;
    }
    let meta_json = serde_json::to_vec_pretty(&meta)
        .map_err(|e| Error::Config(format!("serialize meta.json: {e}")))?;
    up.put_bytes(&marker_key, meta_json, "application/json")
        .await?;
    tracing::info!("archived {} files to {s3_url}", files.len());

    // GitHub Pages publish. Non-fatal: a Pages misconfig must not mask a
    // successful S3 archive — log and continue.
    if do_pages {
        match pages::publish(
            &summary, &meta, &compact, dir, &rid, config, false, opts.force,
        ) {
            Ok(Some(url)) => tracing::info!("published run page: {url}"),
            Ok(None) => {}
            Err(e) => eprintln!("GitHub Pages publish failed (S3 archive succeeded): {e}"),
        }
    }

    Ok(())
}

/// Dataset/raw-vector files are never run artifacts and can be enormous
/// (multi-GB HDF5). Exclude them from the archive even if a results dir
/// happens to contain or sit beside them.
fn is_excluded(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("hdf5" | "h5" | "parquet" | "fvecs" | "bvecs" | "ivecs" | "npy")
    )
}

/// Relative key for a file under `dir`, with forward slashes.
fn rel_key(dir: &Path, file: &Path) -> String {
    file.strip_prefix(dir)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{
        ExtensionInfo, HostInfo, PgRuntimeInfo, RekallInfo, RunEnvironment, SearchResult,
    };

    fn pg_runtime(exts: &[(&str, Option<&str>)]) -> PgRuntimeInfo {
        PgRuntimeInfo {
            version: "PostgreSQL 18.4".into(),
            settings: Default::default(),
            extensions: exts
                .iter()
                .map(|(n, c)| {
                    (
                        n.to_string(),
                        ExtensionInfo {
                            version: "1.0".into(),
                            git_commit: c.map(str::to_string),
                            library_version: None,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn merge_pg_runtime_unions_extensions_preferring_commits() {
        // First file (e.g. an hnsw run) has pgvector but meerkat has no commit;
        // a later mkt run carries meerkat with a real commit.
        let mut base = Some(pg_runtime(&[("vector", None), ("meerkat", None)]));
        merge_pg_runtime(
            &mut base,
            Some(pg_runtime(&[
                ("meerkat", Some("65df24d")),
                ("vchord", Some("abc1234")),
            ])),
        );
        let exts = base.unwrap().extensions;
        // meerkat now carries the commit (replaced the commit-less entry)
        assert_eq!(exts["meerkat"].git_commit.as_deref(), Some("65df24d"));
        // vchord got unioned in
        assert_eq!(exts["vchord"].git_commit.as_deref(), Some("abc1234"));
        // pgvector retained
        assert!(exts.contains_key("vector"));
    }

    #[test]
    fn merge_pg_runtime_adopts_when_base_absent() {
        let mut base = None;
        merge_pg_runtime(&mut base, Some(pg_runtime(&[("meerkat", Some("c"))])));
        assert_eq!(
            base.unwrap().extensions["meerkat"].git_commit.as_deref(),
            Some("c")
        );
    }

    #[test]
    fn load_run_summary_enriches_meerkat_from_per_index_files() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();

        // Consolidated results.json: pg_runtime captured early, before meerkat
        // existed — so it has pgvector but no meerkat.
        let mut consolidated = summary("cohere-10m", "2026-06-10T09:56:42+0000", Some("r8i"), "h");
        consolidated.pg_runtime = Some(pg_runtime(&[("vector", None)]));
        std::fs::write(
            dir.path().join("results.json"),
            serde_json::to_vec(&consolidated).unwrap(),
        )
        .unwrap();

        // Per-index file (written after the meerkat build): has meerkat+commit.
        let mut per_index = summary("cohere-10m", "2026-06-10T09:56:42+0000", Some("r8i"), "h");
        per_index.pg_runtime = Some(pg_runtime(&[
            ("meerkat", Some("65df24d")),
            ("vector", None),
        ]));
        let f =
            std::fs::File::create(dir.path().join("mkt-n16000-20260610-000000.json.gz")).unwrap();
        let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        enc.write_all(&serde_json::to_vec(&per_index).unwrap())
            .unwrap();
        enc.finish().unwrap();

        // Base comes from results.json (no meerkat) but is enriched from the
        // per-index file, so meerkat with its commit is present.
        let loaded = load_run_summary(dir.path()).unwrap();
        let exts = loaded.pg_runtime.unwrap().extensions;
        assert_eq!(exts["meerkat"].git_commit.as_deref(), Some("65df24d"));
        assert!(exts.contains_key("vector"));
    }

    #[test]
    fn load_run_summary_merges_per_index_without_results_json() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let write_gz = |name: &str, s: &RunSummary| {
            let f = std::fs::File::create(dir.path().join(name)).unwrap();
            let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            enc.write_all(&serde_json::to_vec(s).unwrap()).unwrap();
            enc.finish().unwrap();
        };

        // Two self-describing per-index files, NO consolidated results.json
        // (the post-change layout). Each records only its own extension set.
        let mut hnsw = summary("cohere-10m", "2026-06-10T09:56:42+0000", Some("r8i"), "h");
        hnsw.pg_runtime = Some(pg_runtime(&[("vector", None)]));
        hnsw.config = serde_json::json!({"search": {"k": 10, "runs": 3}});
        write_gz("hnsw-m24-20260610-000000.json.gz", &hnsw);

        let mut mkt = summary("cohere-10m", "2026-06-10T09:56:42+0000", Some("r8i"), "h");
        mkt.pg_runtime = Some(pg_runtime(&[
            ("meerkat", Some("65df24d")),
            ("vector", None),
        ]));
        mkt.config = serde_json::json!({"search": {"k": 10, "runs": 3}});
        write_gz("mkt-n16000-20260610-000100.json.gz", &mkt);

        let loaded = load_run_summary(dir.path()).unwrap();
        // Extensions are unioned across the per-index files (meerkat survives).
        let exts = &loaded.pg_runtime.as_ref().unwrap().extensions;
        assert_eq!(exts["meerkat"].git_commit.as_deref(), Some("65df24d"));
        assert!(exts.contains_key("vector"));
        // Config is carried by the self-describing per-index file, so the
        // report's Search section works without a results.json.
        assert_eq!(loaded.config["search"]["k"], serde_json::json!(10));
    }

    fn concurrency(name: &str, clients: usize, qps: f64) -> crate::metrics::ConcurrencyResult {
        crate::metrics::test_concurrency_result(name, 40, clients, qps)
    }

    fn write_gz(dir: &Path, name: &str, s: &RunSummary) {
        use std::io::Write as _;
        let f = std::fs::File::create(dir.join(name)).unwrap();
        let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        enc.write_all(&serde_json::to_vec(s).unwrap()).unwrap();
        enc.finish().unwrap();
    }

    #[test]
    fn load_run_summary_merges_concurrency_results_keeping_newest() {
        let dir = tempfile::tempdir().unwrap();
        let base = || summary("cohere-10m", "2026-06-10T09:56:42+0000", Some("r8i"), "h");

        write_gz(dir.path(), "hnsw-20260610-000000.json.gz", &base());
        let mut older = base();
        older.concurrency_results = vec![concurrency("hnsw", 16, 100.0)];
        write_gz(
            dir.path(),
            "hnsw-concurrency-20260610-000100.json.gz",
            &older,
        );
        let mut newer = base();
        newer.concurrency_results = vec![
            concurrency("hnsw", 16, 200.0),
            concurrency("hnsw", 32, 300.0),
        ];
        write_gz(
            dir.path(),
            "hnsw-concurrency-20260611-000000.json.gz",
            &newer,
        );

        let loaded = load_run_summary(dir.path()).unwrap();
        let got: Vec<(usize, f64)> = loaded
            .concurrency_results
            .iter()
            .map(|r| (r.clients, r.throughput_qps))
            .collect();
        assert_eq!(got, vec![(16, 200.0), (32, 300.0)]);
        // Concurrency files carry no sweep results, so nothing is duplicated.
        assert!(loaded.search_results.is_empty());
    }

    #[test]
    fn load_run_summary_reads_concurrency_results_alongside_results_json() {
        let dir = tempfile::tempdir().unwrap();
        let base = summary("cohere-10m", "2026-06-10T09:56:42+0000", Some("r8i"), "h");
        std::fs::write(
            dir.path().join("results.json"),
            serde_json::to_vec(&base).unwrap(),
        )
        .unwrap();
        let mut conc = base.clone();
        conc.concurrency_results = vec![concurrency("hnsw", 16, 100.0)];
        write_gz(
            dir.path(),
            "hnsw-concurrency-20260610-000100.json.gz",
            &conc,
        );

        let loaded = load_run_summary(dir.path()).unwrap();
        assert_eq!(loaded.concurrency_results.len(), 1);
    }

    fn summary(dataset: &str, ts: &str, instance: Option<&str>, hostname: &str) -> RunSummary {
        RunSummary {
            dataset: dataset.to_string(),
            environment: Some(RunEnvironment {
                host: HostInfo {
                    hostname: hostname.to_string(),
                    os: "Ubuntu".into(),
                    kernel: "6".into(),
                    cpu_model: "Xeon".into(),
                    cpu_count: 8,
                    memory_gb: 61.8,
                    ip_address: None,
                    instance_type: instance.map(str::to_string),
                    storage: None,
                },
                rekall: RekallInfo {
                    version: "0.1.0".into(),
                    git_commit: Some("65df24d".into()),
                    git_branch: None,
                    note: String::new(),
                },
                timestamp: ts.to_string(),
                comment: None,
            }),
            config: serde_json::Value::Null,
            pg_runtime: None,
            index_builds: Vec::new(),
            search_results: Vec::new(),
            concurrency_results: Vec::new(),
            pareto: Vec::new(),
        }
    }

    #[test]
    fn run_id_uses_dataset_date_instance_time() {
        let s = summary(
            "cohere-5m",
            "2026-06-10T09:56:42+0000",
            Some("r8i.2xlarge"),
            "ip-1",
        );
        assert_eq!(
            run_id(&s).unwrap(),
            "cohere-5m/2026-06-10/r8i.2xlarge-095642"
        );
    }

    #[test]
    fn run_id_falls_back_to_hostname_without_instance_type() {
        let s = summary(
            "cohere-5m",
            "2026-06-10T09:56:42+0000",
            None,
            "ip-172-31-4-132",
        );
        assert_eq!(
            run_id(&s).unwrap(),
            "cohere-5m/2026-06-10/ip-172-31-4-132-095642"
        );
    }

    #[test]
    fn run_id_is_deterministic_for_same_run() {
        let a = summary(
            "cohere-5m",
            "2026-06-10T09:56:42+0000",
            Some("r8i.2xlarge"),
            "h",
        );
        let b = summary(
            "cohere-5m",
            "2026-06-10T09:56:42+0000",
            Some("r8i.2xlarge"),
            "h",
        );
        assert_eq!(run_id(&a).unwrap(), run_id(&b).unwrap());
    }

    #[test]
    fn run_id_errors_without_environment() {
        let mut s = summary("cohere-5m", "2026-06-10T09:56:42+0000", None, "h");
        s.environment = None;
        assert!(run_id(&s).is_err());
    }

    #[test]
    fn negative_timezone_time_parses() {
        let s = summary("d", "2026-06-10T23:01:09-0500", Some("i"), "h");
        assert_eq!(run_id(&s).unwrap(), "d/2026-06-10/i-230109");
    }

    #[test]
    fn meta_peak_is_highest_recall_point() {
        let mut s = summary("d", "2026-06-10T09:56:42+0000", Some("i"), "h");
        let mk = |recall: f64, qps: f64| SearchResult {
            index_name: "x".into(),
            index_type: "meerkat".into(),
            data_type: None,
            search_params: serde_json::json!({}),
            k: 10,
            num_queries: 1,
            num_runs: 1,
            avg_recall: recall,
            qps,
            avg_latency_us: 0.0,
            p50_latency_us: 0.0,
            p95_latency_us: 0.0,
            p99_latency_us: 0.0,
            p999_latency_us: 0.0,
            wall_time_secs: 0.0,
            qps_wall: 0.0,
            query_results: Vec::new(),
            cache: None,
            io: None,
        };
        s.search_results = vec![mk(0.90, 500.0), mk(0.99, 120.0), mk(0.95, 300.0)];
        let meta = RunMeta::from_summary(&s, "rid", "s3://b/rid/").unwrap();
        assert_eq!(meta.peak_recall, 0.99);
        assert_eq!(meta.qps_at_peak, 120.0);
    }
}
