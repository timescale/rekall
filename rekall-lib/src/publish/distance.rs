//! Distance-based (ann-benchmarks) recall recompute for the publish flow.
//!
//! Recomputing recall the ann-benchmarks way needs ground-truth distances and
//! the per-query neighbour IDs each index returned. The IDs live in the run's
//! per-index `*.json.gz` files (merged into the summary's `query_results`); the
//! ground truth is sourced, in order, from:
//!   1. an explicit dataset HDF5 (`--dataset`),
//!   2. `ground-truth.json.gz` in the results dir (when it carries distances),
//!   3. the dataset HDF5 resolved from the run's own `[dataset]` config
//!      (usually `~/data/<file>` on a benchmark instance).

use std::path::Path;

use crate::config::DatasetConfig;
use crate::metrics::{recall_distance_based, RunSummary, SearchResult};

/// Ground-truth neighbour IDs and their distances, one row per query.
pub struct GroundTruth {
    pub neighbors: Vec<Vec<i64>>,
    pub distances: Vec<Vec<f32>>,
}

/// Load ground truth with distances, trying (in order) an explicit dataset, the
/// results dir's `ground-truth.json.gz`, then the dataset named in the run's
/// config. Returns `None` if no source with distances is available.
pub fn load_ground_truth(
    dir: &Path,
    dataset_override: Option<&Path>,
    summary: &RunSummary,
) -> Option<GroundTruth> {
    if let Some(ds) = dataset_override {
        if let Some(gt) = from_hdf5(ds) {
            tracing::info!("distance-recall: ground truth from {}", ds.display());
            return Some(gt);
        }
        tracing::warn!(
            "distance-recall: could not read ground truth from {}",
            ds.display()
        );
    }

    let gt_json = dir.join("ground-truth.json.gz");
    if gt_json.exists() {
        if let Some(gt) = from_ground_truth_json(&gt_json) {
            tracing::info!("distance-recall: ground truth from {}", gt_json.display());
            return Some(gt);
        }
        // Present but without distances — fall through to the dataset.
    }

    // Fallback: the dataset file the run was built from (e.g. ~/data/... on the
    // instance), resolved from the recorded [dataset] config.
    if let Some(ds) = dataset_from_config(summary) {
        if ds.exists() {
            if let Some(gt) = from_hdf5(&ds) {
                tracing::info!(
                    "distance-recall: ground truth from dataset {}",
                    ds.display()
                );
                return Some(gt);
            }
        } else {
            tracing::debug!(
                "distance-recall: configured dataset {} not present",
                ds.display()
            );
        }
    }
    None
}

fn dataset_from_config(summary: &RunSummary) -> Option<std::path::PathBuf> {
    let ds: DatasetConfig = serde_json::from_value(summary.config.get("dataset")?.clone()).ok()?;
    Some(ds.resolved_path())
}

fn from_hdf5(path: &Path) -> Option<GroundTruth> {
    let file = hdf5::File::open(path).ok()?;
    // neighbours may be stored as i32 or i64.
    let neighbors = file.dataset("neighbors").ok().and_then(|d| {
        d.read_2d::<i64>()
            .ok()
            .or_else(|| d.read_2d::<i32>().ok().map(|a| a.mapv(|v| v as i64)))
    })?;
    let distances = file.dataset("distances").ok()?.read_2d::<f32>().ok()?;
    Some(GroundTruth {
        neighbors: neighbors.outer_iter().map(|r| r.to_vec()).collect(),
        distances: distances.outer_iter().map(|r| r.to_vec()).collect(),
    })
}

fn from_ground_truth_json(path: &Path) -> Option<GroundTruth> {
    use std::io::Read as _;
    #[derive(serde::Deserialize)]
    struct Gt {
        neighbors: Vec<Vec<i64>>,
        #[serde(default)]
        distances: Option<Vec<Vec<f32>>>,
    }
    let f = std::fs::File::open(path).ok()?;
    let mut s = String::new();
    flate2::read::GzDecoder::new(f)
        .read_to_string(&mut s)
        .ok()?;
    let gt: Gt = serde_json::from_str(&s).ok()?;
    let distances = gt.distances?; // distance method needs distances
    Some(GroundTruth {
        neighbors: gt.neighbors,
        distances,
    })
}

/// Recompute each result's `avg_recall` with the distance method, using the
/// per-query IDs in `query_results`. Results without saved per-query IDs are
/// dropped (can't be recomputed). Returns the rebuilt results (query_results
/// cleared) ready for plotting.
pub fn recompute(summary: &RunSummary, gt: &GroundTruth) -> Vec<SearchResult> {
    const EPSILON: f64 = 1e-3;
    let mut out = Vec::new();
    for r in &summary.search_results {
        if r.query_results.is_empty() {
            continue;
        }
        let nq = r.query_results.len().min(gt.neighbors.len());
        if nq == 0 {
            continue;
        }
        let mut total = 0.0;
        for qi in 0..nq {
            total += recall_distance_based(
                &r.query_results[qi].neighbors,
                &gt.neighbors[qi],
                &gt.distances[qi],
                r.k,
                EPSILON,
            );
        }
        let mut updated = r.clone();
        updated.avg_recall = total / nq as f64;
        updated.query_results = Vec::new();
        out.push(updated);
    }
    out
}
