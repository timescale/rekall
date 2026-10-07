//! Compact per-run summary data (`summary.json`): the numbers the run-page
//! comparison tables and the catalog's Meerkat-vs-competition column need,
//! a few KB per run. Written into the results dir (so it is archived to S3)
//! and into the run's directory on the gh-pages branch, where the catalog
//! reads it back at render time — method changes therefore re-render the
//! whole catalog on the next publish without touching old runs.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::metrics::{RunSummary, SearchResult};

use super::distance;

pub const SUMMARY_FILE: &str = "summary.json";

pub use crate::metrics::RecallBasis;

/// Search results carrying their FINAL recall values, computed once per
/// publish and shared by report generation, `summary.json`, and the run-page
/// tables — so every published number for a run agrees on the recall basis.
pub struct FinalResults {
    pub basis: RecallBasis,
    /// `query_results` cleared; qps/latency preserved.
    pub results: Vec<SearchResult>,
}

impl FinalResults {
    /// ID-basis results without touching ground truth (used by dry-run).
    pub fn id_basis(summary: &RunSummary) -> Self {
        let results = summary
            .search_results
            .iter()
            .map(|r| {
                let mut r = r.clone();
                r.query_results = Vec::new();
                r
            })
            .collect();
        FinalResults {
            basis: RecallBasis::Id,
            results,
        }
    }
}

/// Compute the run's final results: the distance-based recompute when ground
/// truth is resolvable (explicit `--dataset`, the run dir's
/// `ground-truth.json.gz`, or the run's configured dataset file) and at least
/// one result saved its per-query IDs; otherwise the ID-based results.
pub fn compute_final_results(
    summary: &RunSummary,
    dir: &Path,
    dataset: Option<&Path>,
) -> FinalResults {
    if let Some(gt) = distance::load_ground_truth(dir, dataset, summary) {
        let results = distance::recompute(summary, &gt);
        if !results.is_empty() {
            return FinalResults {
                basis: RecallBasis::Distance,
                results,
            };
        }
        tracing::info!("distance recall: no per-query results saved, using ID-based recall");
    }
    FinalResults::id_basis(summary)
}

/// One search sweep point (an index config at one search-parameter setting).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepPoint {
    pub index_name: String,
    pub index_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    pub recall: f64,
    pub qps: f64,
    pub p50_latency_us: f64,
    pub p95_latency_us: f64,
    pub p99_latency_us: f64,
}

/// One index build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildInfo {
    pub index_name: String,
    pub index_type: String,
    pub build_time_secs: f64,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
}

/// The `summary.json` payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactSummary {
    pub recall_basis: RecallBasis,
    pub points: Vec<SweepPoint>,
    pub builds: Vec<BuildInfo>,
}

impl CompactSummary {
    pub fn build(summary: &RunSummary, finals: &FinalResults) -> Self {
        let points = finals
            .results
            .iter()
            .map(|r| SweepPoint {
                index_name: r.index_name.clone(),
                index_type: r.index_type.clone(),
                data_type: r.data_type.clone(),
                recall: r.avg_recall,
                qps: r.qps,
                p50_latency_us: r.p50_latency_us,
                p95_latency_us: r.p95_latency_us,
                p99_latency_us: r.p99_latency_us,
            })
            .collect();
        let builds = summary
            .index_builds
            .iter()
            .map(|b| BuildInfo {
                index_name: b.index_name.clone(),
                index_type: b.index_type.clone(),
                build_time_secs: b.build_time.as_secs_f64(),
                size_bytes: b.index_size_bytes,
                cache: b.cache.clone(),
            })
            .collect();
        CompactSummary {
            recall_basis: finals.basis,
            points,
            builds,
        }
    }
}

/// Write `summary.json` into `dir`.
pub fn write_summary_json(dir: &Path, cs: &CompactSummary) -> Result<()> {
    let json = serde_json::to_vec_pretty(cs)
        .map_err(|e| Error::Config(format!("serialize {SUMMARY_FILE}: {e}")))?;
    std::fs::write(dir.join(SUMMARY_FILE), json)
        .map_err(|e| Error::Config(format!("write {SUMMARY_FILE}: {e}")))
}

/// Read a run's `summary.json` from `dir`, `None` when absent or unreadable
/// (older runs published before summaries existed).
pub fn read_summary_json(dir: &Path) -> Option<CompactSummary> {
    let s = std::fs::read_to_string(dir.join(SUMMARY_FILE)).ok()?;
    serde_json::from_str(&s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn search_result(name: &str, recall: f64, qps: f64) -> SearchResult {
        SearchResult {
            index_name: name.into(),
            index_type: "prism".into(),
            data_type: None,
            search_params: serde_json::json!({"probes": 10}),
            k: 10,
            num_queries: 100,
            num_runs: 3,
            avg_recall: recall,
            qps,
            avg_latency_us: 300.0,
            p50_latency_us: 280.0,
            p95_latency_us: 440.0,
            p99_latency_us: 510.0,
            p999_latency_us: 700.0,
            wall_time_secs: 0.0,
            qps_wall: 0.0,
            query_results: vec![crate::metrics::QueryResult {
                neighbors: vec![1, 2, 3],
                recall: 1.0,
            }],
            cache: None,
            io: None,
        }
    }

    fn run_summary() -> RunSummary {
        RunSummary {
            dataset: "cohere-1m".into(),
            environment: None,
            config: serde_json::Value::Null,
            pg_runtime: None,
            index_builds: vec![crate::metrics::IndexBuildResult {
                index_name: "mkt-n1000".into(),
                index_type: "prism".into(),
                params: serde_json::json!({"nlist": 1000}),
                build_time: Duration::from_secs_f64(312.5),
                index_size_bytes: 3_100_000_000,
                parallel_workers: 8,
                create_sql: None,
                build_gucs: serde_json::Value::Null,
                monitor: None,
                cache: Some("prewarmed".into()),
            }],
            search_results: vec![search_result("mkt-n1000", 0.91, 3000.0)],
            concurrency_results: Vec::new(),
            pareto: Vec::new(),
        }
    }

    #[test]
    fn compact_summary_strips_configs_and_converts_units() {
        let s = run_summary();
        let finals = FinalResults::id_basis(&s);
        let cs = CompactSummary::build(&s, &finals);
        assert_eq!(cs.recall_basis, RecallBasis::Id);
        assert_eq!(cs.points.len(), 1);
        let p = &cs.points[0];
        assert_eq!(p.index_name, "mkt-n1000");
        assert_eq!(p.recall, 0.91);
        assert_eq!(p.p95_latency_us, 440.0);
        let b = &cs.builds[0];
        assert_eq!(b.build_time_secs, 312.5);
        assert_eq!(b.size_bytes, 3_100_000_000);
        assert_eq!(b.cache.as_deref(), Some("prewarmed"));
        // No search params / query results / configs anywhere in the payload.
        let json = serde_json::to_string(&cs).unwrap();
        assert!(!json.contains("search_params"));
        assert!(!json.contains("query_results"));
    }

    #[test]
    fn recall_basis_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&RecallBasis::Distance).unwrap(),
            "\"distance\""
        );
        assert_eq!(serde_json::to_string(&RecallBasis::Id).unwrap(), "\"id\"");
    }

    #[test]
    fn summary_json_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = run_summary();
        let cs = CompactSummary::build(&s, &FinalResults::id_basis(&s));
        write_summary_json(dir.path(), &cs).unwrap();
        let back = read_summary_json(dir.path()).unwrap();
        assert_eq!(back.recall_basis, RecallBasis::Id);
        assert_eq!(back.points.len(), 1);
        assert_eq!(back.builds.len(), 1);
    }

    #[test]
    fn read_summary_json_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_summary_json(dir.path()).is_none());
    }

    #[test]
    fn id_basis_clears_query_results() {
        let s = run_summary();
        let finals = FinalResults::id_basis(&s);
        assert!(finals.results[0].query_results.is_empty());
        assert_eq!(finals.results[0].p50_latency_us, 280.0);
    }
}
