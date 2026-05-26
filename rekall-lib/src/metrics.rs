use std::collections::HashSet;
use std::time::Duration;

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub cpu_model: String,
    pub cpu_count: usize,
    pub memory_gb: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RekallInfo {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEnvironment {
    pub host: HostInfo,
    pub rekall: RekallInfo,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub neighbors: Vec<i64>,
    pub recall: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub index_name: String,
    pub index_type: String,
    pub search_params: serde_json::Value,
    pub k: usize,
    pub num_queries: usize,
    pub num_runs: usize,
    pub avg_recall: f64,
    pub qps: f64,
    pub avg_latency_us: f64,
    pub p50_latency_us: f64,
    pub p95_latency_us: f64,
    pub p99_latency_us: f64,
    pub p999_latency_us: f64,
    #[serde(default)]
    pub wall_time_secs: f64,
    #[serde(default)]
    pub qps_wall: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_results: Vec<QueryResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexBuildResult {
    pub index_name: String,
    pub index_type: String,
    pub params: serde_json::Value,
    pub build_time: Duration,
    pub index_size_bytes: u64,
    #[serde(default)]
    pub parallel_workers: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<crate::backend::build_monitor::BuildMonitorResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParetoPoint {
    pub index_name: String,
    pub search_params: serde_json::Value,
    pub recall: f64,
    pub qps: f64,
    pub is_pareto: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionInfo {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PgRuntimeInfo {
    pub version: String,
    pub settings: std::collections::BTreeMap<String, String>,
    pub extensions: std::collections::BTreeMap<String, ExtensionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub dataset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<RunEnvironment>,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default)]
    pub pg_runtime: Option<PgRuntimeInfo>,
    #[serde(default)]
    pub index_builds: Vec<IndexBuildResult>,
    #[serde(default)]
    pub search_results: Vec<SearchResult>,
    #[serde(default)]
    pub pareto: Vec<ParetoPoint>,
}

pub fn compute_recall(result_ids: &[i64], ground_truth: &[i64], k: usize) -> f64 {
    let gt_set: HashSet<i64> = ground_truth.iter().take(k).copied().collect();
    let result_set: HashSet<i64> = result_ids.iter().take(k).copied().collect();
    let hits = gt_set.intersection(&result_set).count();
    hits as f64 / k as f64
}

pub struct LatencyCollector {
    histogram: Histogram<u64>,
}

impl Default for LatencyCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyCollector {
    pub fn new() -> Self {
        Self {
            histogram: Histogram::new_with_bounds(1, 60_000_000, 3).unwrap(),
        }
    }

    pub fn record(&mut self, latency_us: u64) {
        let _ = self.histogram.record(latency_us);
    }

    pub fn mean_us(&self) -> f64 {
        self.histogram.mean()
    }

    pub fn percentile_us(&self, p: f64) -> f64 {
        self.histogram.value_at_percentile(p) as f64
    }

    pub fn count(&self) -> u64 {
        self.histogram.len()
    }

    pub fn reset(&mut self) {
        self.histogram.reset();
    }
}

pub fn compute_pareto(results: &[SearchResult]) -> Vec<ParetoPoint> {
    let mut points: Vec<ParetoPoint> = results
        .iter()
        .map(|r| ParetoPoint {
            index_name: r.index_name.clone(),
            search_params: r.search_params.clone(),
            recall: r.avg_recall,
            qps: r.qps,
            is_pareto: false,
        })
        .collect();

    points.sort_by(|a, b| {
        a.recall
            .partial_cmp(&b.recall)
            .unwrap()
            .then(b.qps.partial_cmp(&a.qps).unwrap())
    });

    let mut best_qps = f64::NEG_INFINITY;
    for point in points.iter_mut().rev() {
        if point.qps >= best_qps {
            point.is_pareto = true;
            best_qps = point.qps;
        }
    }

    points
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_perfect() {
        let result = vec![1, 2, 3, 4, 5];
        let gt = vec![1, 2, 3, 4, 5];
        assert_eq!(compute_recall(&result, &gt, 5), 1.0);
    }

    #[test]
    fn recall_none() {
        let result = vec![6, 7, 8, 9, 10];
        let gt = vec![1, 2, 3, 4, 5];
        assert_eq!(compute_recall(&result, &gt, 5), 0.0);
    }

    #[test]
    fn recall_partial() {
        let result = vec![1, 2, 6, 7, 8];
        let gt = vec![1, 2, 3, 4, 5];
        assert_eq!(compute_recall(&result, &gt, 5), 0.4);
    }

    #[test]
    fn recall_uses_k_not_vec_len() {
        let result = vec![1, 2, 3, 4, 5, 6, 7];
        let gt = vec![1, 2, 3, 10, 20, 30, 40];
        assert_eq!(compute_recall(&result, &gt, 3), 1.0);
    }

    #[test]
    fn recall_order_independent() {
        let result = vec![5, 3, 1, 4, 2];
        let gt = vec![1, 2, 3, 4, 5];
        assert_eq!(compute_recall(&result, &gt, 5), 1.0);
    }

    #[test]
    fn latency_collector_basic() {
        let mut c = LatencyCollector::new();
        c.record(100);
        c.record(200);
        c.record(300);
        assert_eq!(c.count(), 3);
        let mean = c.mean_us();
        assert!((mean - 200.0).abs() < 5.0);
    }

    #[test]
    fn latency_collector_percentiles() {
        let mut c = LatencyCollector::new();
        for i in 1..=1000 {
            c.record(i);
        }
        let p50 = c.percentile_us(50.0);
        assert!((490.0..=510.0).contains(&p50));
        let p99 = c.percentile_us(99.0);
        assert!((985.0..=1000.0).contains(&p99));
    }

    #[test]
    fn latency_collector_reset() {
        let mut c = LatencyCollector::new();
        c.record(100);
        assert_eq!(c.count(), 1);
        c.reset();
        assert_eq!(c.count(), 0);
    }

    fn make_search_result(name: &str, recall: f64, qps: f64) -> SearchResult {
        SearchResult {
            index_name: name.to_string(),
            index_type: "test".to_string(),
            search_params: serde_json::json!({}),
            k: 10,
            num_queries: 100,
            num_runs: 1,
            avg_recall: recall,
            qps,
            avg_latency_us: 1e6 / qps,
            p50_latency_us: 0.0,
            p95_latency_us: 0.0,
            p99_latency_us: 0.0,
            p999_latency_us: 0.0,
            wall_time_secs: 100.0 / qps,
            qps_wall: qps,
            query_results: Vec::new(),
        }
    }

    #[test]
    fn pareto_empty() {
        let pareto = compute_pareto(&[]);
        assert!(pareto.is_empty());
    }

    #[test]
    fn pareto_single() {
        let results = vec![make_search_result("a", 0.9, 1000.0)];
        let pareto = compute_pareto(&results);
        assert_eq!(pareto.len(), 1);
        assert!(pareto[0].is_pareto);
    }

    #[test]
    fn pareto_dominated() {
        // b dominates a (higher recall AND higher QPS)
        let results = vec![
            make_search_result("a", 0.8, 500.0),
            make_search_result("b", 0.9, 1000.0),
        ];
        let pareto = compute_pareto(&results);
        let pareto_points: Vec<_> = pareto.iter().filter(|p| p.is_pareto).collect();
        assert_eq!(pareto_points.len(), 1);
        assert_eq!(pareto_points[0].index_name, "b");
    }

    #[test]
    fn pareto_tradeoff() {
        // a has higher QPS, b has higher recall — both on frontier
        let results = vec![
            make_search_result("a", 0.8, 2000.0),
            make_search_result("b", 0.95, 500.0),
        ];
        let pareto = compute_pareto(&results);
        let pareto_names: Vec<_> = pareto
            .iter()
            .filter(|p| p.is_pareto)
            .map(|p| p.index_name.as_str())
            .collect();
        assert_eq!(pareto_names.len(), 2);
        assert!(pareto_names.contains(&"a"));
        assert!(pareto_names.contains(&"b"));
    }

    #[test]
    fn pareto_frontier_correct() {
        let results = vec![
            make_search_result("a", 0.7, 3000.0),
            make_search_result("b", 0.8, 2000.0),
            make_search_result("c", 0.85, 1500.0), // dominated by b+d
            make_search_result("d", 0.9, 1800.0),
            make_search_result("e", 0.95, 500.0),
        ];
        let pareto = compute_pareto(&results);
        let frontier: Vec<_> = pareto
            .iter()
            .filter(|p| p.is_pareto)
            .map(|p| p.index_name.as_str())
            .collect();
        assert!(frontier.contains(&"a"));
        assert!(frontier.contains(&"d"));
        assert!(frontier.contains(&"e"));
    }
}
