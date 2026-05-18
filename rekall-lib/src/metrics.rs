use std::collections::HashSet;
use std::time::Duration;

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexBuildResult {
    pub index_name: String,
    pub index_type: String,
    pub params: serde_json::Value,
    pub build_time: Duration,
    pub index_size_bytes: u64,
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
pub struct RunSummary {
    pub dataset: String,
    pub index_builds: Vec<IndexBuildResult>,
    pub search_results: Vec<SearchResult>,
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
