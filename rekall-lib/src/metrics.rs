use std::collections::HashSet;
use std::time::Duration;

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

/// Display label for an index family: the readable index-type name plus the
/// data-type variant, e.g. "PRISM", "pgvector HNSW (bit)". Single source of
/// truth for plot series grouping and the published comparison tables — the
/// two must agree so a table column is findable in the plots.
pub fn family_label(index_type: &str, data_type: Option<&str>) -> String {
    let family = match index_type {
        "prism" | "meerkat" => "PRISM",
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
    /// Observed storage backing the PG data directory. Absent on older
    /// result files and where the device could not be identified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageInfo>,
}

/// What the benchmark actually ran on, storage-wise, read from the machine
/// rather than from any config file.
///
/// This exists because a config is a request, not a fact. `disk_iops = 16000`
/// in an instance config was silently dropped by the native AWS launch path
/// and every such box ran on the gp3 3000-IOPS baseline; nothing in the
/// published results said so, and a whole benchmark rung was measured against
/// a volume nobody knew was the bottleneck. Every field here is observed on
/// the running instance at benchmark time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageInfo {
    /// PG's own `data_directory`, as reported by the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_directory: Option<String>,
    /// Mount point the data directory resolves to, and the filesystem on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount_point: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<String>,
    /// What kind of storage this actually IS, determined by inspecting the
    /// device -- never assumed from what was provisioned. One of
    /// "ebs", "instance-store-nvme", "local-nvme", "local-disk",
    /// "rotational", or "unknown". A run configured for instance-store NVMe
    /// that silently landed on the EBS root volume (or the reverse) is
    /// exactly the mistake this field exists to catch.
    pub media: String,
    /// Block device backing the PG data directory (e.g. "nvme0n1").
    pub device: String,
    /// Whether `device` was resolved from PG's own data_directory (true) or
    /// guessed from the whole-disk list (false).
    pub device_resolved_from_pgdata: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_gb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotational: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduler: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nr_requests: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_ahead_kb: Option<u32>,
    /// EBS volume id, decoded from the NVMe device serial -- readable from
    /// inside the instance with no AWS API call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ebs_volume_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ebs_volume_type: Option<String>,
    /// PROVISIONED IOPS, not achieved. The achieved rate is in the per-point
    /// disk samples; this is the ceiling the volume was configured with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ebs_provisioned_iops: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ebs_provisioned_throughput_mbs: Option<u32>,
    /// Where the ebs_* values came from, or why they are absent. Recorded so
    /// a missing IOPS number is never mistaken for a default one.
    pub ebs_source: String,
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

/// How recall was counted: distance-based (ann-benchmarks, a result that
/// ties the k-th true neighbour's distance counts) or by exact neighbour ID.
/// A published run is all one basis, distance-based when ground-truth
/// distances were available at publish time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecallBasis {
    Distance,
    Id,
}

impl RecallBasis {
    /// Human wording for table captions.
    pub fn describe(self) -> &'static str {
        match self {
            RecallBasis::Distance => "distance-based (ann-benchmarks, tie-aware)",
            RecallBasis::Id => "ID-based",
        }
    }
}

/// One measurement with several concurrent clients at a recall target's
/// operating point. Kept apart from `SearchResult` so single-client
/// reports, plots and Pareto frontiers never mix in results measured
/// under load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrencyResult {
    pub index_name: String,
    pub index_type: String,
    #[serde(default)]
    pub data_type: Option<String>,
    /// Recall targets these params are the operating point for.
    pub recall_targets: Vec<f64>,
    pub search_params: serde_json::Value,
    pub clients: usize,
    /// The `search.clients` entry that resolved to `clients` (e.g. "2x").
    /// `None` for the single-client baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients_spec: Option<String>,
    /// CPUs on the rekall host when `clients` was resolved.
    pub cpu_count: usize,
    pub k: usize,
    pub num_queries: usize,
    pub num_runs: usize,
    /// Mean recall over the first pass through the query set, on
    /// `recall_basis`.
    pub avg_recall: f64,
    /// Distance-based when the dataset has ground-truth distances, as in
    /// the published reports, else ID-based.
    pub recall_basis: RecallBasis,
    /// Sum over clients of (queries it finished before the first client
    /// found no query left / time of the last of them): the rate while
    /// every client is busy, with no query cut in half.
    pub throughput_qps: f64,
    /// `queries_finished_total / wall_time_total_secs`, drain included.
    pub qps_wall: f64,
    /// Time from the start until the first client found no query left.
    pub wall_time_till_first_empty_secs: f64,
    /// Time from the start until the last client finished.
    pub wall_time_total_secs: f64,
    pub queries_finished_till_first_empty: u64,
    pub queries_finished_total: u64,
    pub avg_latency_us: f64,
    pub p50_latency_us: f64,
    pub p95_latency_us: f64,
    pub p99_latency_us: f64,
    pub p999_latency_us: f64,
    /// CPU time (user + system) rekall itself used during the timed run.
    pub client_cpu_secs: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io: Option<SearchIoStats>,
}

impl ConcurrencyResult {
    /// Whether this result measured `clients` clients on `index_name` at
    /// `search_params`, with the same k and query count. Used to resume
    /// and to deduplicate results found in several files.
    pub fn measures(
        &self,
        index_name: &str,
        search_params: &serde_json::Value,
        clients: usize,
        k: usize,
        num_queries: usize,
    ) -> bool {
        self.index_name == index_name
            && self.search_params == *search_params
            && self.clients == clients
            && self.k == k
            && self.num_queries == num_queries
    }
}

/// Each series' concurrency results from its newest concurrency file only:
/// a series' newest file is its complete current set, so results for
/// operating points it no longer selects are left behind in older files.
/// `files` are (file name, results) in name order; names end in a
/// timestamp, so later is newer. Other result files are ignored.
pub fn latest_concurrency_results(
    files: Vec<(String, Vec<ConcurrencyResult>)>,
) -> Vec<ConcurrencyResult> {
    let mut latest: Vec<(String, Vec<ConcurrencyResult>)> = Vec::new();
    for (name, results) in files {
        let Some(series) = crate::config::concurrency_file_series(&name) else {
            continue;
        };
        latest.retain(|(s, _)| s != series);
        latest.push((series.to_string(), results));
    }
    latest
        .into_iter()
        .flat_map(|(_, results)| results)
        .collect()
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
    /// `max_parallel_maintenance_workers`, `prism.fastscan_bits`).
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub concurrency_results: Vec<ConcurrencyResult>,
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
/// neighbour within `epsilon` of the k-th true neighbour's value counts as
/// correct, so a returned point that ties the k-th true neighbour isn't
/// penalized. Returns 0 if the ground truth has fewer than `k` entries.
///
/// Ground-truth rows are best-first, but the stored values may be either
/// distances (ascending, e.g. ann-benchmarks) or similarities (descending,
/// e.g. the inner-product LAION files); the row's own ordering determines
/// which side of the k-th value counts as "at least as good".
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
    let similarity = (gt_distances[0] as f64) > (gt_distances[k - 1] as f64);
    let kth = gt_distances[k - 1] as f64;
    let correct_set: HashSet<i64> = gt_neighbors
        .iter()
        .zip(gt_distances.iter())
        .filter(|(_, d)| {
            let d = **d as f64;
            if similarity {
                d >= kth - epsilon
            } else {
                d <= kth + epsilon
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

/// Tie tolerance for distance-based recall, as in ann-benchmarks.
pub const DISTANCE_RECALL_EPSILON: f64 = 1e-3;

/// Distance-based recall averaged over a sweep point's per-query results.
/// `gt_row(qi)` gives query `qi`'s ground-truth neighbours and distances;
/// queries it has no row for are skipped. `None` when no query has one.
pub fn mean_recall_distance_based<'a>(
    query_results: &[QueryResult],
    k: usize,
    gt_row: impl Fn(usize) -> Option<(&'a [i64], &'a [f32])>,
) -> Option<f64> {
    let mut total = 0.0;
    let mut counted = 0usize;
    for (qi, qr) in query_results.iter().enumerate() {
        if let Some((neighbors, distances)) = gt_row(qi) {
            total += recall_distance_based(
                &qr.neighbors,
                neighbors,
                distances,
                k,
                DISTANCE_RECALL_EPSILON,
            );
            counted += 1;
        }
    }
    (counted > 0).then(|| total / counted as f64)
}

/// The operating point at a recall target: the highest-QPS item whose
/// recall reaches `target`. `None` when no item reaches it.
pub fn operating_point<T>(
    items: impl IntoIterator<Item = T>,
    target: f64,
    recall: impl Fn(&T) -> f64,
    qps: impl Fn(&T) -> f64,
) -> Option<T> {
    items
        .into_iter()
        .filter(|item| recall(item) >= target)
        .max_by(|a, b| qps(a).total_cmp(&qps(b)))
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

    /// Add every latency recorded in `other`, as if recorded here.
    pub fn merge(&mut self, other: &LatencyCollector) {
        self.histogram
            .add(&other.histogram)
            .expect("collectors share the same histogram bounds");
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

/// A concurrency result for tests: `clients` clients on series `name` at
/// `prism.nprobe = nprobe`, measuring `qps`.
#[cfg(test)]
pub(crate) fn test_concurrency_result(
    name: &str,
    nprobe: i64,
    clients: usize,
    qps: f64,
) -> ConcurrencyResult {
    ConcurrencyResult {
        index_name: name.to_string(),
        index_type: "prism".to_string(),
        data_type: None,
        recall_targets: vec![0.95],
        search_params: serde_json::json!({ "prism.nprobe": nprobe }),
        clients,
        clients_spec: Some("2x".to_string()),
        cpu_count: 8,
        k: 10,
        num_queries: 1000,
        num_runs: 3,
        avg_recall: 0.96,
        recall_basis: RecallBasis::Distance,
        throughput_qps: qps,
        qps_wall: qps,
        wall_time_till_first_empty_secs: 1.0,
        wall_time_total_secs: 1.0,
        queries_finished_till_first_empty: 1000,
        queries_finished_total: 1000,
        avg_latency_us: 0.0,
        p50_latency_us: 0.0,
        p95_latency_us: 0.0,
        p99_latency_us: 0.0,
        p999_latency_us: 0.0,
        client_cpu_secs: 0.0,
        cache: None,
        io: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_label_maps_known_types() {
        assert_eq!(family_label("prism", None), "PRISM");
        assert_eq!(family_label("meerkat", None), "PRISM");
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
    fn recall_distance_based_ascending_distances() {
        // Distance GT (best-first, ascending): id 6 ties the k-th value
        // within epsilon and counts as correct.
        let gt_ids = vec![1, 2, 3, 4, 5, 6];
        let gt_d = vec![0.10, 0.20, 0.30, 0.40, 0.50, 0.5005];
        let result = vec![1, 2, 3, 4, 6];
        assert_eq!(recall_distance_based(&result, &gt_ids, &gt_d, 5, 1e-3), 1.0);
        // id 6 outside epsilon does not count.
        let gt_d = vec![0.10, 0.20, 0.30, 0.40, 0.50, 0.51];
        assert_eq!(recall_distance_based(&result, &gt_ids, &gt_d, 5, 1e-3), 0.8);
    }

    #[test]
    fn recall_distance_based_descending_similarities() {
        // Similarity GT (best-first, descending, e.g. inner-product LAION):
        // ties resolve on the opposite side of the k-th value.
        let gt_ids = vec![1, 2, 3, 4, 5, 6];
        let gt_s = vec![0.90, 0.80, 0.70, 0.60, 0.50, 0.4995];
        let result = vec![1, 2, 3, 4, 6];
        assert_eq!(recall_distance_based(&result, &gt_ids, &gt_s, 5, 1e-3), 1.0);
        // Beyond epsilon: only the 4 true top-5 hits count.
        let gt_s = vec![0.90, 0.80, 0.70, 0.60, 0.50, 0.49];
        assert_eq!(recall_distance_based(&result, &gt_ids, &gt_s, 5, 1e-3), 0.8);
        // Deep GT entries far below the k-th similarity must NOT count
        // (the pre-fix behavior admitted the entire tail).
        let gt_ids = vec![1, 2, 3, 4, 5, 100, 200];
        let gt_s = vec![0.90, 0.80, 0.70, 0.60, 0.50, 0.30, 0.10];
        let result = vec![100, 200, 7, 8, 9];
        assert_eq!(recall_distance_based(&result, &gt_ids, &gt_s, 5, 1e-3), 0.0);
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
    #[test]
    fn latest_concurrency_results_keep_each_series_newest_file() {
        let file = |name: &str, results: Vec<ConcurrencyResult>| (name.to_string(), results);
        let latest = latest_concurrency_results(vec![
            // a's older file still has a point a's newer file dropped.
            file(
                "a-concurrency-20260610-000000.json.gz",
                vec![
                    test_concurrency_result("a", 10, 16, 100.0),
                    test_concurrency_result("a", 20, 16, 200.0),
                ],
            ),
            file(
                "a-concurrency-20260611-000000.json.gz",
                vec![test_concurrency_result("a", 20, 16, 300.0)],
            ),
            // b's newest file is empty: no point reaches a target any more.
            file(
                "b-concurrency-20260610-000000.json.gz",
                vec![test_concurrency_result("b", 10, 16, 400.0)],
            ),
            file("b-concurrency-20260612-000000.json.gz", vec![]),
            file(
                "c-concurrency-20260610-000000.json.gz",
                vec![test_concurrency_result("c", 10, 16, 500.0)],
            ),
            // Sweep files never contribute.
            file(
                "c-20260613-000000.json.gz",
                vec![test_concurrency_result("c", 10, 16, 999.0)],
            ),
        ]);
        let qps: Vec<(&str, f64)> = latest
            .iter()
            .map(|r| (r.index_name.as_str(), r.throughput_qps))
            .collect();
        assert_eq!(qps, vec![("a", 300.0), ("c", 500.0)]);
    }

    #[test]
    fn run_summary_without_concurrency_results_still_reads() {
        let old = r#"{"dataset": "d", "search_results": [], "pareto": []}"#;
        let summary: RunSummary = serde_json::from_str(old).unwrap();
        assert!(summary.concurrency_results.is_empty());
        // And an empty list is not written back out.
        let json = serde_json::to_value(&summary).unwrap();
        assert!(json.get("concurrency_results").is_none());
    }

    #[test]
    fn concurrency_result_round_trips() {
        let r = test_concurrency_result("a", 10, 16, 123.0);
        let back: ConcurrencyResult =
            serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
        assert!(back.measures("a", &r.search_params, 16, 10, 1000));
        assert_eq!(back.clients_spec.as_deref(), Some("2x"));
        assert_eq!(back.throughput_qps, 123.0);
    }

    #[test]
    fn operating_point_takes_highest_qps_reaching_target() {
        let points = [(0.94, 900.0), (0.95, 500.0), (0.97, 700.0), (0.99, 100.0)];
        let op = operating_point(points.iter(), 0.95, |p| p.0, |p| p.1);
        assert_eq!(op, Some(&(0.97, 700.0)));
        assert_eq!(
            operating_point(points.iter(), 0.995, |p| p.0, |p| p.1),
            None
        );
    }

    #[test]
    fn mean_recall_distance_based_averages_rows_with_ground_truth() {
        // k = 2. Query 0 returns both true neighbours (recall 1.0); query 1
        // returns one of them (0.5); query 2 has no ground-truth row and is
        // skipped rather than counted as zero.
        let neighbors = [vec![1_i64, 2, 3], vec![4, 5, 6]];
        let distances = [vec![0.1_f32, 0.2, 0.9], vec![0.1, 0.2, 0.9]];
        let results = [
            QueryResult {
                neighbors: vec![2, 1],
                recall: 0.0,
            },
            QueryResult {
                neighbors: vec![4, 9],
                recall: 0.0,
            },
            QueryResult {
                neighbors: vec![7, 8],
                recall: 0.0,
            },
        ];
        let mean = mean_recall_distance_based(&results, 2, |qi| {
            Some((neighbors.get(qi)?.as_slice(), distances.get(qi)?.as_slice()))
        });
        assert_eq!(mean, Some(0.75));
    }

    #[test]
    fn mean_recall_distance_based_none_without_ground_truth() {
        let results = [QueryResult {
            neighbors: vec![1],
            recall: 0.0,
        }];
        assert_eq!(mean_recall_distance_based(&results, 1, |_| None), None);
    }
    #[test]
    fn latency_collector_merge_matches_recording_everything_in_one() {
        // A skewed distribution split unevenly across three clients.
        let values: Vec<u64> = (1..=2000).map(|i| i * i % 7919 + 1).collect();
        let mut direct = LatencyCollector::new();
        let mut parts = [
            LatencyCollector::new(),
            LatencyCollector::new(),
            LatencyCollector::new(),
        ];
        for (i, v) in values.iter().enumerate() {
            direct.record(*v);
            parts[i % 7 % 3].record(*v);
        }
        let mut merged = LatencyCollector::new();
        for p in &parts {
            merged.merge(p);
        }
        assert_eq!(merged.count(), 2000);
        assert_eq!(merged.mean_us(), direct.mean_us());
        for p in [50.0, 95.0, 99.0, 99.9] {
            assert_eq!(merged.percentile_us(p), direct.percentile_us(p), "p{p}");
        }
    }
}
