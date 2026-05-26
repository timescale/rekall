use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::time::Instant;

use tracing::info;

use crate::backend::BackendRegistry;
use crate::config::{BenchmarkConfig, CacheMode};
use crate::dataset::hdf5::Hdf5Reader;
use crate::dataset::DatasetReader;
use crate::error::{Error, Result};
use crate::metrics::{
    compute_recall, IndexBuildResult, LatencyCollector, PgRuntimeInfo, QueryResult, RunEnvironment,
    RunSummary, SearchResult,
};
use crate::progress::{ProgressEvent, ProgressTx};

fn write_json_gz(path: &str, value: &impl serde::Serialize) -> std::io::Result<()> {
    let json = serde_json::to_vec(value).map_err(|e| std::io::Error::other(e.to_string()))?;
    let file = std::fs::File::create(path)?;
    let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    encoder.write_all(&json)?;
    encoder.finish()?;
    Ok(())
}

fn read_json_file(path: &std::path::Path) -> Option<RunSummary> {
    let ext = path.extension()?.to_str()?;
    if ext == "gz" {
        let file = std::fs::File::open(path).ok()?;
        let mut decoder = flate2::read::GzDecoder::new(file);
        let mut json = String::new();
        decoder.read_to_string(&mut json).ok()?;
        serde_json::from_str(&json).ok()
    } else if ext == "json" {
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    } else {
        None
    }
}

/// Check if a result already exists for this index + params combination.
fn is_completed(
    existing: &[SearchResult],
    index_name: &str,
    params: &HashMap<String, toml::Value>,
) -> bool {
    existing.iter().any(|r| {
        r.index_name == index_name
            && r.search_params == serde_json::to_value(params).unwrap_or_default()
    })
}

/// Load previously completed results from per-index result files in results_dir.
/// Scans for {index_name}-*.json and {index_name}-*.json.gz files.
pub fn load_existing_results(config: &BenchmarkConfig) -> Vec<SearchResult> {
    let dir = &config.output.results_dir;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut all = Vec::new();

    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.to_string_lossy();
            name.ends_with(".json") || name.ends_with(".json.gz")
        })
        .collect();
    files.sort();

    for path in &files {
        let Some(summary) = read_json_file(path) else {
            continue;
        };
        all.extend(summary.search_results);
    }

    // Also check legacy json_path
    if let Some(path) = &config.output.json_path {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(summary) = serde_json::from_str::<crate::metrics::RunSummary>(&content) {
                all.extend(summary.search_results);
            }
        }
    }

    all
}

/// Save ground truth to a separate file for offline recall recomputation.
/// Written once per run — skipped if the file already exists.
fn save_ground_truth(
    config: &BenchmarkConfig,
    ground_truth: &ndarray::Array2<i64>,
    gt_distances: Option<&ndarray::Array2<f32>>,
    num_queries: usize,
    k: usize,
) {
    let dir = &config.output.results_dir;
    let path = format!("{dir}/ground-truth.json.gz");

    if std::path::Path::new(&path).exists() {
        return;
    }
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let neighbors: Vec<Vec<i64>> = (0..num_queries)
        .map(|qi| ground_truth.row(qi).iter().take(k).copied().collect())
        .collect();

    let distances: Option<Vec<Vec<f32>>> = gt_distances.map(|d| {
        (0..num_queries)
            .map(|qi| d.row(qi).iter().take(k).copied().collect())
            .collect()
    });

    #[derive(serde::Serialize)]
    struct GroundTruth {
        dataset: String,
        k: usize,
        num_queries: usize,
        neighbors: Vec<Vec<i64>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        distances: Option<Vec<Vec<f32>>>,
    }

    let data = GroundTruth {
        dataset: config.dataset.name.clone(),
        k,
        num_queries,
        neighbors,
        distances,
    };

    match write_json_gz(&path, &data) {
        Ok(()) => tracing::info!("Ground truth saved to {path}"),
        Err(e) => tracing::error!("Failed to save ground truth to {path}: {e}"),
    }
}

/// Save results for a specific index to its own timestamped file.
fn save_index_results(
    config: &BenchmarkConfig,
    index_name: &str,
    timestamp: &str,
    all_results: &[SearchResult],
    all_builds: &[IndexBuildResult],
    environment: Option<&RunEnvironment>,
    pg_runtime: Option<&PgRuntimeInfo>,
) {
    let index_results: Vec<_> = all_results
        .iter()
        .filter(|r| r.index_name == index_name)
        .cloned()
        .collect();

    if index_results.is_empty() {
        return;
    }

    let index_builds: Vec<_> = all_builds
        .iter()
        .filter(|b| b.index_name == index_name)
        .cloned()
        .collect();

    let path = config.output.index_results_path(index_name, timestamp);
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let summary = crate::metrics::RunSummary {
        dataset: config.dataset.name.clone(),
        environment: environment.cloned(),
        config: serde_json::Value::Null,
        pg_runtime: pg_runtime.cloned(),
        index_builds,
        search_results: index_results,
        pareto: Vec::new(),
    };

    match write_json_gz(&path, &summary) {
        Ok(()) => tracing::info!("Results saved to {path}"),
        Err(e) => tracing::error!("Failed to save results to {path}: {e}"),
    }
}

pub async fn run(
    config: &BenchmarkConfig,
    registry: &BackendRegistry,
    progress: &ProgressTx,
    filter_index: Option<&str>,
    environment: Option<&RunEnvironment>,
    pg_runtime: Option<&PgRuntimeInfo>,
    index_builds: &[IndexBuildResult],
) -> Result<Vec<SearchResult>> {
    let dataset_path = config.dataset.resolved_path();
    let reader = Hdf5Reader::open(
        &dataset_path,
        config.dataset.metric,
        config.dataset.dimension,
    )?;

    let queries = reader.queries()?;
    let ground_truth = reader.ground_truth()?;
    let gt_distances = reader.ground_truth_distances()?;

    let num_queries = config
        .search
        .num_queries
        .unwrap_or(queries.nrows())
        .min(queries.nrows());

    let k = config.search.k;

    save_ground_truth(config, &ground_truth, gt_distances.as_ref(), num_queries, k);

    let existing = load_existing_results(config);
    let mut all_results = existing.clone();
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();

    for sweep in &config.search.sweep {
        if let Some(name) = filter_index {
            if sweep.index != name {
                continue;
            }
        }

        let idx_config = config
            .index
            .iter()
            .find(|i| i.name == sweep.index)
            .ok_or_else(|| Error::SweepIndexNotFound {
                name: sweep.index.clone(),
            })?;

        // Disable all other rekall indexes, enable only the active one
        let mut pg_mgmt = crate::backend::postgres::PgConnection::new(
            &config.connection,
            &config.table_name(),
            config.dataset.dimension,
            config.dataset.metric,
        );
        pg_mgmt.connect().await?;
        let active_idx = pg_mgmt.index_sql_name(&sweep.index);
        let all_indexes = pg_mgmt.list_rekall_indexes().await?;
        for idx_sql in &all_indexes {
            if *idx_sql == active_idx {
                pg_mgmt.enable_index(idx_sql).await?;
            } else {
                pg_mgmt.disable_index(idx_sql).await?;
            }
        }

        let backend = registry
            .create(
                &config.connection,
                &config.table_name(),
                config.dataset.dimension,
                config.dataset.metric,
                idx_config,
                None,
            )
            .await?;

        if !backend.index_exists().await? {
            return Err(Error::IndexNotFound {
                name: sweep.index.clone(),
            });
        }

        // Validate default GUCs from index config
        let default_gucs_for_validation: HashMap<String, Vec<toml::Value>> = idx_config
            .gucs
            .iter()
            .map(|(k, v)| (k.clone(), vec![v.clone()]))
            .collect();
        if !default_gucs_for_validation.is_empty() {
            backend
                .validate_search_params(&default_gucs_for_validation)
                .await?;
        }
        backend.validate_search_params(&sweep.params).await?;

        // Validate query plan uses the expected index
        backend
            .validate_query_plan(&queries.row(0).to_vec(), k)
            .await?;
        let _ = progress.send(ProgressEvent::Message {
            text: format!("Query plan validated for '{}'", sweep.index),
        });

        let param_combos = expand_sweep_params(&sweep.params);

        for params in &param_combos {
            let params_json = serde_json::to_value(params).unwrap_or_default();

            // Skip if already completed in a previous run
            if is_completed(&all_results, &sweep.index, params) {
                let _ = progress.send(ProgressEvent::Message {
                    text: format!(
                        "Skipping '{}' {} (already completed)",
                        sweep.index, params_json
                    ),
                });
                continue;
            }

            let _ = progress.send(ProgressEvent::SearchStart {
                index_name: sweep.index.clone(),
                params: params_json.clone(),
            });

            // Apply default GUCs from index config first, then sweep overrides
            backend.set_search_params(&idx_config.gucs).await?;
            backend.set_search_params(params).await?;

            match config.search.cache {
                CacheMode::Warm => {
                    // Warmup queries to fill shared buffers
                    for qi in 0..config.search.warmup.min(num_queries) {
                        let q = queries.row(qi).to_vec();
                        let _ = backend.query(&q, k).await?;
                    }
                }
                CacheMode::Cold => {
                    // Evict shared buffers before measurement.
                    // Skip warmup to measure cold-cache performance.
                    let mut pg_evict = crate::backend::postgres::PgConnection::new(
                        &config.connection,
                        &config.table_name(),
                        config.dataset.dimension,
                        config.dataset.metric,
                    );
                    pg_evict.connect().await?;
                    let idx_sql = pg_evict.index_sql_name(&sweep.index);
                    pg_evict
                        .evict_relation_buffers(&config.table_name())
                        .await?;
                    pg_evict.evict_relation_buffers(&idx_sql).await?;

                    let _ = progress.send(ProgressEvent::Message {
                        text: "Evicted shared buffers (cold mode)".to_string(),
                    });
                }
            }

            let mut best_collector = LatencyCollector::new();
            let mut best_avg = f64::MAX;
            let mut best_wall_secs = 0.0;
            let mut total_recall = 0.0;
            let mut query_results: Vec<QueryResult> = Vec::new();

            for run in 0..config.search.runs {
                let mut collector = LatencyCollector::new();
                let mut run_recall = 0.0;

                let wall_start = Instant::now();
                for qi in 0..num_queries {
                    let q = queries.row(qi).to_vec();
                    let gt = ground_truth.row(qi).to_vec();

                    let start = Instant::now();
                    let result_ids = backend.query(&q, k).await?;
                    let latency = start.elapsed();

                    collector.record(latency.as_micros() as u64);

                    if run == 0 {
                        let recall = compute_recall(&result_ids, &gt, k);
                        run_recall += recall;
                        query_results.push(QueryResult {
                            neighbors: result_ids,
                            recall,
                        });
                    }
                }
                let wall_secs = wall_start.elapsed().as_secs_f64();

                if run == 0 {
                    total_recall = run_recall / num_queries as f64;
                }

                let avg = collector.mean_us();
                if avg < best_avg {
                    best_avg = avg;
                    best_collector = collector;
                    best_wall_secs = wall_secs;
                }
            }

            let qps = if best_avg > 0.0 {
                1_000_000.0 / best_avg
            } else {
                0.0
            };
            let qps_wall = if best_wall_secs > 0.0 {
                num_queries as f64 / best_wall_secs
            } else {
                0.0
            };

            let result = SearchResult {
                index_name: sweep.index.clone(),
                index_type: idx_config.index_type.clone(),
                search_params: params_json.clone(),
                k,
                num_queries,
                num_runs: config.search.runs,
                avg_recall: total_recall,
                qps,
                avg_latency_us: best_collector.mean_us(),
                p50_latency_us: best_collector.percentile_us(50.0),
                p95_latency_us: best_collector.percentile_us(95.0),
                p99_latency_us: best_collector.percentile_us(99.0),
                p999_latency_us: best_collector.percentile_us(99.9),
                wall_time_secs: best_wall_secs,
                qps_wall,
                query_results,
            };

            let _ = progress.send(ProgressEvent::SearchEnd {
                index_name: sweep.index.clone(),
                recall: result.avg_recall,
                qps: result.qps,
            });

            info!(
                "  {} [{}]: recall={:.4} QPS={:.0}",
                sweep.index, params_json, result.avg_recall, result.qps
            );

            all_results.push(result);

            // Save incrementally per-index so progress survives crashes
            save_index_results(
                config,
                &sweep.index,
                &timestamp,
                &all_results,
                index_builds,
                environment,
                pg_runtime,
            );
        }
    }

    // Re-enable all indexes when done
    let mut pg_cleanup = crate::backend::postgres::PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );
    pg_cleanup.connect().await?;
    for idx_sql in pg_cleanup.list_rekall_indexes().await? {
        let _ = pg_cleanup.enable_index(&idx_sql).await;
    }

    Ok(all_results)
}

fn expand_sweep_params(
    params: &HashMap<String, Vec<toml::Value>>,
) -> Vec<HashMap<String, toml::Value>> {
    let keys: Vec<&String> = params.keys().collect();
    let values: Vec<&Vec<toml::Value>> = keys.iter().map(|k| &params[*k]).collect();

    if keys.is_empty() {
        return vec![HashMap::new()];
    }

    let mut combos = Vec::new();
    let mut indices = vec![0usize; keys.len()];

    loop {
        let mut combo = HashMap::new();
        for (i, key) in keys.iter().enumerate() {
            combo.insert((*key).clone(), values[i][indices[i]].clone());
        }
        combos.push(combo);

        // Increment indices (Cartesian product)
        let mut carry = true;
        for i in (0..keys.len()).rev() {
            if carry {
                indices[i] += 1;
                if indices[i] >= values[i].len() {
                    indices[i] = 0;
                } else {
                    carry = false;
                }
            }
        }
        if carry {
            break;
        }
    }

    combos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_empty_params() {
        let params = HashMap::new();
        let combos = expand_sweep_params(&params);
        assert_eq!(combos.len(), 1);
        assert!(combos[0].is_empty());
    }

    #[test]
    fn expand_single_param() {
        let mut params = HashMap::new();
        params.insert(
            "mkt.nprobe".to_string(),
            vec![
                toml::Value::Integer(5),
                toml::Value::Integer(10),
                toml::Value::Integer(20),
            ],
        );
        let combos = expand_sweep_params(&params);
        assert_eq!(combos.len(), 3);
        let values: Vec<i64> = combos
            .iter()
            .map(|c| c["mkt.nprobe"].as_integer().unwrap())
            .collect();
        assert!(values.contains(&5));
        assert!(values.contains(&10));
        assert!(values.contains(&20));
    }

    #[test]
    fn expand_cartesian_product() {
        let mut params = HashMap::new();
        params.insert(
            "a".to_string(),
            vec![toml::Value::Integer(1), toml::Value::Integer(2)],
        );
        params.insert(
            "b".to_string(),
            vec![
                toml::Value::Integer(10),
                toml::Value::Integer(20),
                toml::Value::Integer(30),
            ],
        );
        let combos = expand_sweep_params(&params);
        assert_eq!(combos.len(), 6); // 2 * 3
    }

    #[test]
    fn expand_single_value() {
        let mut params = HashMap::new();
        params.insert("x".to_string(), vec![toml::Value::Integer(42)]);
        let combos = expand_sweep_params(&params);
        assert_eq!(combos.len(), 1);
        assert_eq!(combos[0]["x"].as_integer().unwrap(), 42);
    }
}
