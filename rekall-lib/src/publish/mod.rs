//! Publishing: archive a run's results directory to S3 and push HTML reports
//! + a catalog to GitHub Pages.
//!
//! The run-id is **deterministic from the run's recorded metadata** (dataset,
//! run-start date/time, instance), so re-publishing a run maps to the same
//! S3 prefix / Pages entry rather than creating a duplicate.

mod distance;
mod pages;
mod report;
mod s3;

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::PublishConfig;
use crate::error::{Error, Result};
use crate::metrics::RunSummary;

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

/// Load the best available `RunSummary` for a results dir: prefer the
/// consolidated `results.json`, else merge the per-index `*.json.gz` files.
///
/// In both cases the per-index files' `pg_runtime.extensions` are unioned into
/// the result. The consolidated `results.json` captures `pg_runtime` once,
/// early in the run — often before `CREATE EXTENSION meerkat` runs in the index
/// phase — so it can omit meerkat's version/commit entirely. The per-index
/// files (written after each build) do record it, so merging them in restores
/// the full extension set for the report's Versions table and plot captions.
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
    // extension's creation.
    for p in &gz {
        if let Ok(s) = read_summary_file(p) {
            merge_pg_runtime(&mut base.pg_runtime, s.pg_runtime);
        }
    }
    Ok(base)
}

/// Merge one per-index file's `pg_runtime` into the accumulator. Different
/// index types load different extensions (an hnsw run records pgvector but not
/// meerkat, and vice-versa), so the union across files is what reflects the
/// whole run. Adopt `pg_runtime` if absent; otherwise union the extensions,
/// preferring an entry that carries a git commit over one that doesn't.
fn merge_pg_runtime(
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

    // Step 1: generate the report HTML/SVG into the results dir so they are
    // archived to S3 *and* available for the Pages copy. (Skipped on dry-run.)
    if !opts.dry_run {
        report::generate_into_dir(&summary, dir, opts.dataset.as_deref());
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
        println!("  {s3_url}meta.json  (completion marker, written last)");
        if do_pages {
            pages::publish(&summary, &meta, dir, &rid, config, true)?;
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
        match pages::publish(&summary, &meta, dir, &rid, config, false) {
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
                },
                rekall: RekallInfo {
                    version: "0.1.0".into(),
                    git_commit: Some("65df24d".into()),
                    git_branch: None,
                    note: String::new(),
                },
                timestamp: ts.to_string(),
            }),
            config: serde_json::Value::Null,
            pg_runtime: None,
            index_builds: Vec::new(),
            search_results: Vec::new(),
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
        };
        s.search_results = vec![mk(0.90, 500.0), mk(0.99, 120.0), mk(0.95, 300.0)];
        let meta = RunMeta::from_summary(&s, "rid", "s3://b/rid/").unwrap();
        assert_eq!(meta.peak_recall, 0.99);
        assert_eq!(meta.qps_at_peak, 120.0);
    }
}
