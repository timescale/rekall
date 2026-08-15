use std::collections::HashSet;
use std::time::Duration;

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

/// Display label for an index family: the readable index-type name plus the
/// data-type variant, e.g. "Meerkat", "pgvector HNSW (bit)". Single source of
/// truth for plot series grouping and the published comparison tables — the
/// two must agree so a table column is findable in the plots.
pub fn family_label(index_type: &str, data_type: Option<&str>) -> String {
    let family = match index_type {
        "meerkat" => "Meerkat",
        "pgvector-hnsw" => "pgvector HNSW",
        other => other,
    };
    match data_type {
        Some(dt) => format!("{family} ({dt})"),
        None => family.to_string(),
    }
}

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
    /// Cloud instance type (e.g. "r8i.2xlarge"), when detectable from the
    /// AWS EC2 metadata service or set via `REKALL_INSTANCE_TYPE`. Absent
    /// for most local/bare-metal runs, and missing from older result files
    /// (hence `default` on read) — the hardware fields above are always
    /// present, so plots show them alongside the instance type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_type: Option<String>,
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
    /// Free-form description of what this run was for
    /// (`rekall run --comment`). Carried through results files and shown
    /// on reports, plots, and published pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub neighbors: Vec<i64>,
    pub recall: f64,
}

/// One interval sample of the block device(s) backing the PG data
/// directory. All values are deltas over the sample interval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskIoSample {
    pub elapsed_ms: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub reads: u64,
    pub writes: u64,
    /// Time the device spent with I/O in flight during the interval
    /// (diskstats "time doing I/O"); busy% = io_time_ms / interval.
    pub io_time_ms: u64,
}

/// I/O done by the query backend over one sweep point, from PG 18's
/// `pg_stat_get_backend_io()`, summed across objects and contexts.
/// Unlike the per-relation counters this is exact per-backend
/// attribution (nothing from autovacuum, checkpointer, or the monitor).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendIo {
    /// Shared-buffer misses that became read() calls to the OS. These
    /// may still be served from the OS page cache — compare with the
    /// device-level counters to see what actually hit disk.
    pub reads: i64,
    pub read_bytes: i64,
    /// Shared-buffer hits (no syscall).
    pub hits: i64,
    pub writes: i64,
    pub write_bytes: i64,
    pub evictions: i64,
    /// Time spent in reads (ms). `None` unless track_io_timing was on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_time_ms: Option<f64>,
}

/// I/O statistics for one search sweep point, covering all timed runs
/// (`queries` = num_queries × runs; warmup and cache eviction excluded).
///
/// Three layers, from PostgreSQL down to hardware:
///  - per-relation shared-buffer traffic for the active index and heap
///    (`pg_statio_*` deltas; one block = 8 kB),
///  - the query backend's own I/O (`backend`, PG 18+),
///  - the block device backing the data directory (`disk_*`), which is
///    what physically hit storage after the OS page cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchIoStats {
    /// Timed queries covered — the denominator for per-query rates.
    pub queries: u64,
    /// Wall time covered by the disk counters.
    pub duration_ms: u64,
    pub idx_blks_read: i64,
    pub idx_blks_hit: i64,
    pub heap_blks_read: i64,
    pub heap_blks_hit: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendIo>,
    /// Device sampled (e.g. "nvme0n1p1"). `None` means the data
    /// directory's device could not be resolved and the `disk_*`
    /// counters aggregate all whole-disk devices on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_device: Option<String>,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub disk_reads: u64,
    pub disk_writes: u64,
    pub disk_io_time_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disk_samples: Vec<DiskIoSample>,
}

impl SearchIoStats {
    /// Shared-buffer hit ratio, preferring exact backend attribution
    /// when available, else the per-relation counters. `None` when no
    /// buffer was touched (avoids a fake 100%/0%).
    pub fn shared_hit_ratio(&self) -> Option<f64> {
        let (hits, reads) = match &self.backend {
            Some(b) => (b.hits, b.reads),
            None => (
                self.idx_blks_hit + self.heap_blks_hit,
                self.idx_blks_read + self.heap_blks_read,
            ),
        };
        let total = hits + reads;
        (total > 0).then(|| hits as f64 / total as f64)
    }

    pub fn disk_read_bytes_per_query(&self) -> f64 {
        if self.queries == 0 {
            return 0.0;
        }
        self.disk_read_bytes as f64 / self.queries as f64
    }

    /// Shared-buffer misses (8 kB blocks read from the OS) per query.
    pub fn blks_read_per_query(&self) -> f64 {
        if self.queries == 0 {
            return 0.0;
        }
        (self.idx_blks_read + self.heap_blks_read) as f64 / self.queries as f64
    }

    /// Fraction of the wall time the device had I/O in flight.
    pub fn disk_busy_ratio(&self) -> Option<f64> {
        (self.duration_ms > 0).then(|| self.disk_io_time_ms as f64 / self.duration_ms as f64)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub index_name: String,
    pub index_type: String,
    /// Data-type/variant descriptor that distinguishes Pareto series within
    /// one index family (e.g. pgvector's `bit` vs `halfvec` vs full
    /// `vector`). `None` groups the result under its bare algorithm family.
    #[serde(default)]
    pub data_type: Option<String>,
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
    /// Cache state actually applied before this point ("warm", "cold",
    /// "disk-cold", or "cold (OS cache drop failed)" when disk-cold was
    /// requested but the host command was denied). May differ from the
    /// configured mode — reports must trust this, not the config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    /// I/O statistics for this sweep point (buffer + backend + device).
    /// Absent when io_stats is disabled or on result files that predate
    /// the feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io: Option<SearchIoStats>,
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
    /// The exact `CREATE INDEX ...` statement that built this index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_sql: Option<String>,
    /// Build-session GUCs applied before `CREATE INDEX` (e.g.
    /// `max_parallel_maintenance_workers`, `mkt.fastscan_bits`).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub build_gucs: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<crate::backend::build_monitor::BuildMonitorResult>,
    /// Cache state applied before this build ("prewarmed" or "cold"). Recorded
    /// so build-time comparisons and their reports state the conditions;
    /// absent on runs that predate per-build cache control (build order then
    /// determined how warm the heap was).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
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

/// ann-benchmarks-style recall@k that handles distance ties: any ground-truth
/// neighbour within `gt_distances[k-1] + epsilon` counts as correct, so a
/// returned point that ties the k-th true neighbour isn't penalized. Returns
/// 0 if the ground truth has fewer than `k` entries.
///
/// `gt_distances` is ordered best-first (index 0 = nearest neighbour), but
/// "best" means smallest for a true distance (L2, cosine distance) and
/// largest for a raw similarity (inner product). Direction is inferred from
/// the data itself rather than assumed, so datasets whose ground truth is
/// stored as descending similarity (e.g. inner-product ground truth reused
/// under a euclidean-metric benchmark) aren't scored backwards -- that bug
/// previously collapsed every engine's recall to ~k-th-rank-only regardless
/// of tuning.
pub fn recall_distance_based(
    result_ids: &[i64],
    gt_neighbors: &[i64],
    gt_distances: &[f32],
    k: usize,
    epsilon: f64,
) -> f64 {
    if gt_distances.len() < k || gt_neighbors.len() < k {
        return 0.0;
    }
    let kth = gt_distances[k - 1] as f64;
    let ascending = gt_distances[0] as f64 <= kth;
    let correct_set: HashSet<i64> = gt_neighbors
        .iter()
        .zip(gt_distances.iter())
        .filter(|(_, d)| {
            let d = **d as f64;
            if ascending {
                d <= kth + epsilon
            } else {
                d >= kth - epsilon
            }
        })
        .map(|(id, _)| *id)
        .collect();
    let hits = result_ids
        .iter()
        .take(k)
        .filter(|id| correct_set.contains(id))
        .count();
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
    fn family_label_maps_known_types() {
        assert_eq!(family_label("meerkat", None), "Meerkat");
        assert_eq!(family_label("pgvector-hnsw", None), "pgvector HNSW");
        assert_eq!(family_label("vchordrq", None), "vchordrq");
    }

    #[test]
    fn family_label_appends_data_type() {
        assert_eq!(
            family_label("pgvector-hnsw", Some("bit")),
            "pgvector HNSW (bit)"
        );
        assert_eq!(family_label("custom", Some("halfvec")), "custom (halfvec)");
    }

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
            data_type: None,
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
            cache: None,
            io: None,
        }
    }

    #[test]
    fn search_result_deserializes_without_io_field() {
        // Result files that predate I/O stats must keep loading.
        let mut v = serde_json::to_value(make_search_result("a", 0.9, 100.0)).unwrap();
        v.as_object_mut().unwrap().remove("io");
        let r: SearchResult = serde_json::from_value(v).unwrap();
        assert!(r.io.is_none());
    }

    #[test]
    fn io_stats_hit_ratio_prefers_backend() {
        let mut io = SearchIoStats {
            queries: 10,
            idx_blks_read: 50,
            idx_blks_hit: 50,
            ..Default::default()
        };
        // Per-relation only: 50/(50+50).
        assert_eq!(io.shared_hit_ratio(), Some(0.5));
        // Exact backend counters win when present.
        io.backend = Some(BackendIo {
            reads: 10,
            hits: 90,
            ..Default::default()
        });
        assert_eq!(io.shared_hit_ratio(), Some(0.9));
    }

    #[test]
    fn io_stats_ratios_none_or_zero_when_empty() {
        let io = SearchIoStats::default();
        assert_eq!(io.shared_hit_ratio(), None);
        assert_eq!(io.disk_busy_ratio(), None);
        assert_eq!(io.disk_read_bytes_per_query(), 0.0);
        assert_eq!(io.blks_read_per_query(), 0.0);
    }

    #[test]
    fn io_stats_per_query_rates() {
        let io = SearchIoStats {
            queries: 100,
            duration_ms: 2000,
            idx_blks_read: 300,
            heap_blks_read: 200,
            disk_read_bytes: 100 * 8192,
            disk_io_time_ms: 1000,
            ..Default::default()
        };
        assert_eq!(io.blks_read_per_query(), 5.0);
        assert_eq!(io.disk_read_bytes_per_query(), 8192.0);
        assert_eq!(io.disk_busy_ratio(), Some(0.5));
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
