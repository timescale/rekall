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

/// On a crash/disconnect error, write a diagnostics report and surface its
/// path. No-op for ordinary query errors (see [`crate::diagnostics`]).
fn report_crash(config: &BenchmarkConfig, progress: &ProgressTx, context: &str, err: &Error) {
    if let Some(path) = crate::diagnostics::report_if_crash(
        &config.output.results_dir,
        &config.connection.dsn,
        context,
        err,
    ) {
        tracing::error!(
            "crash/disconnect during {context}; diagnostics: {}",
            path.display()
        );
        let _ = progress.send(ProgressEvent::Message {
            text: format!(
                "crash/disconnect during {context} — diagnostics written to {}",
                path.display()
            ),
        });
    }
}

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

    all
}

/// Probe the live PG runtime (version, settings, installed extensions with
/// their version/git commit) on a throwaway connection. Called at search time
/// so the captured extension set reflects everything built so far. Returns
/// `None` if the probe connection can't be opened.
async fn capture_runtime_info(config: &BenchmarkConfig) -> Option<PgRuntimeInfo> {
    let mut pg = crate::backend::postgres::PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );
    if pg.connect().await.is_err() {
        return None;
    }
    pg.runtime_info().await.ok()
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

/// Save one result series to its own timestamped file. `series_name` labels
/// the search results (a query name); `build_index_name` is the physical
/// index the series searched, used to attach its build stats (the two differ
/// when several named queries target one index).
#[allow(clippy::too_many_arguments)]
fn save_index_results(
    config: &BenchmarkConfig,
    series_name: &str,
    build_index_name: &str,
    timestamp: &str,
    all_results: &[SearchResult],
    all_builds: &[IndexBuildResult],
    environment: Option<&RunEnvironment>,
    pg_runtime: Option<&PgRuntimeInfo>,
) {
    let index_results: Vec<_> = all_results
        .iter()
        .filter(|r| r.index_name == series_name)
        .cloned()
        .collect();

    if index_results.is_empty() {
        return;
    }

    let index_builds: Vec<_> = all_builds
        .iter()
        .filter(|b| b.index_name == build_index_name)
        .cloned()
        .collect();

    let path = config.output.index_results_path(series_name, timestamp);
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let summary = crate::metrics::RunSummary {
        dataset: config.dataset.name.clone(),
        environment: environment.cloned(),
        // Carry the run config so each per-index file is self-describing: the
        // report/publish aggregator reads `config.search` (k/runs/warmup/
        // num_queries) for the run's Search section, now that there is no
        // consolidated results.json. Cheap next to the query results.
        config: serde_json::to_value(config).unwrap_or(serde_json::Value::Null),
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

#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: &BenchmarkConfig,
    registry: &BackendRegistry,
    progress: &ProgressTx,
    filter_index: Option<&str>,
    environment: Option<&RunEnvironment>,
    pg_runtime: Option<&PgRuntimeInfo>,
    index_builds: &[IndexBuildResult],
    rerun: bool,
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

    // With --rerun, ignore previously-saved results so every sweep point is
    // re-measured. Otherwise the run is resumable: completed points are
    // loaded and skipped (see `is_completed`).
    let existing = if rerun {
        Vec::new()
    } else {
        load_existing_results(config)
    };
    let mut all_results = existing.clone();
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();

    // Capture the PG runtime now, at search time, rather than trusting the
    // snapshot threaded in from run start. By now every index has been built,
    // so all extensions under test exist in the catalog — meaning each
    // per-index file records the correct extension version + git commit +
    // settings. The run-start snapshot predates `CREATE EXTENSION` in the
    // index phase, so it omits e.g. meerkat entirely. In the interleaved
    // build+search path this runs once per index (filter_index is set), so
    // each file is captured independently. Falls back to the threaded value
    // if the probe connection fails.
    let captured_runtime = capture_runtime_info(config).await;
    let pg_runtime = captured_runtime.as_ref().or(pg_runtime);

    // Flush the index phase's write debt (dirty index pages, un-checkpointed
    // WAL) before any timed queries. CHECKPOINT is synchronous, so once it
    // returns the first search arm is no longer competing with the last
    // build's background writeback for disk bandwidth. In the interleaved
    // build+search path this runs once per arm; after the first there is
    // nothing left to flush and it returns immediately. Without superuser or
    // the pg_checkpoint role it is denied: warn and continue, at the cost of
    // the first arm racing the checkpointer for a while.
    {
        let mut pg = crate::backend::postgres::PgConnection::new(
            &config.connection,
            &config.table_name(),
            config.dataset.dimension,
            config.dataset.metric,
        );
        pg.connect().await?;
        match pg.checkpoint().await {
            Ok(secs) => {
                info!("Checkpoint before queries: {secs:.1}s");
                let _ = progress.send(ProgressEvent::Message {
                    text: format!("Checkpoint before queries: {secs:.1}s"),
                });
            }
            Err(e) => {
                tracing::warn!(
                    "CHECKPOINT before queries failed ({e}); the first \
                     search arm will compete with leftover build writeback \
                     — grant the pg_checkpoint role to avoid this"
                );
                let _ = progress.send(ProgressEvent::Message {
                    text: "CHECKPOINT before queries denied — grant the \
                           pg_checkpoint role so query runs don't compete \
                           with leftover build writeback"
                        .to_string(),
                });
            }
        }
    }

    for query in &config.effective_queries() {
        // `filter_index` is a physical-index filter (used by the interleaved
        // build+search path); match it against the index this query targets.
        if let Some(name) = filter_index {
            if query.index != name {
                continue;
            }
        }

        let idx_config = config
            .index
            .iter()
            .find(|i| i.name == query.index)
            .ok_or_else(|| Error::SweepIndexNotFound {
                name: query.index.clone(),
            })?;

        // Disable all other rekall indexes, enable only the active one
        let mut pg_mgmt = crate::backend::postgres::PgConnection::new(
            &config.connection,
            &config.table_name(),
            config.dataset.dimension,
            config.dataset.metric,
        );
        // Management connections are built outside create_pg_backend, so
        // carry the arm's physical-index redirection explicitly or the
        // SQL name resolves to a relation that does not exist.
        pg_mgmt.set_physical_index(idx_config.physical_index.clone());
        pg_mgmt.connect().await?;
        let active_idx = pg_mgmt.index_sql_name(&query.index);
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
                query.mode,
                None,
            )
            .await?;

        if !backend.index_exists().await? {
            return Err(Error::IndexNotFound {
                name: query.index.clone(),
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
        backend.validate_search_params(&query.params).await?;

        // Validate query plan uses the expected index
        backend
            .validate_query_plan(&queries.row(0).to_vec(), k)
            .await?;
        let _ = progress.send(ProgressEvent::Message {
            text: format!("Query plan validated for '{}'", query.name),
        });

        // Data-type label (e.g. pgvector `bit`/`halfvec`) — constant per
        // backend; recorded on each result to separate Pareto series.
        let data_type = backend.data_type_label();

        // I/O monitor for this query series: one extra connection that
        // snapshots PG counters and samples the data-dir block device
        // around each sweep point's timed runs. The backend pid pins
        // pg_stat_get_backend_io to the connection running the queries.
        let io_monitor = if config.search.io_stats {
            let backend_pid = backend
                .pg()
                .client()
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .ok()
                .map(|r| r.get::<_, i32>(0));
            let m = crate::backend::search_monitor::SearchIoMonitor::new(
                &config.connection.dsn,
                &active_idx,
                &config.table_name(),
                backend_pid,
                std::time::Duration::from_millis(config.search.io_sample_interval_ms),
            )
            .await;
            if m.is_none() {
                tracing::warn!(
                    "io stats: monitor connection failed; disabled for '{}'",
                    query.name
                );
            }
            m
        } else {
            None
        };

        let param_combos = expand_sweep_params(&query.params);

        // Warm the sweep's widest parameter setting once before any timed
        // point runs. Each point's own warmup below only touches the
        // narrow slice that point's own setting visits — at the lowest
        // ef_search/nprobe in the sweep that's a small fraction of the
        // index, so the first (lowest-recall) point ends up measuring a
        // largely cold index while later, wider points organically finish
        // warming it as they go (observed directly: a cold hnsw-halfvec
        // index took ~250k block reads at ef_search=10 down to 0 by
        // ef_search=800, and the low-recall points looked artificially
        // slow enough to get Pareto-dominated). Pre-touching the widest
        // setting's traversal once, up front, pulls the index's real
        // working set into cache before anything is measured, instead of
        // charging that cost to the first sweep point.
        if config.search.cache == CacheMode::Warm
            && param_combos.len() > 1
            && param_combos
                .iter()
                .any(|p| !is_completed(&all_results, &query.name, p))
        {
            let broadest = broadest_params(&param_combos);
            backend.set_search_params(&idx_config.gucs).await?;
            backend.set_search_params(&broadest).await?;
            for qi in 0..config.search.warmup.min(num_queries) {
                let q = queries.row(qi).to_vec();
                if let Err(e) = backend.query(&q, k).await {
                    report_crash(
                        config,
                        progress,
                        &format!("search '{}' broad warmup query #{qi}", query.name),
                        &e,
                    );
                    return Err(e);
                }
            }
        }

        for params in &param_combos {
            let params_json = serde_json::to_value(params).unwrap_or_default();

            // Skip if already completed in a previous run
            if is_completed(&all_results, &query.name, params) {
                let _ = progress.send(ProgressEvent::Message {
                    text: format!(
                        "Skipping '{}' {} (already completed)",
                        query.name, params_json
                    ),
                });
                continue;
            }

            let _ = progress.send(ProgressEvent::SearchStart {
                index_name: query.name.clone(),
                params: params_json.clone(),
            });

            // Apply default GUCs from index config first, then sweep overrides
            backend.set_search_params(&idx_config.gucs).await?;
            backend.set_search_params(params).await?;

            let mut effective_cache = match config.search.cache {
                CacheMode::Warm => "warm",
                CacheMode::Cold => "cold",
                CacheMode::DiskCold => "disk-cold",
            };
            match config.search.cache {
                CacheMode::Warm => {
                    // Warmup queries to fill shared buffers
                    for qi in 0..config.search.warmup.min(num_queries) {
                        let q = queries.row(qi).to_vec();
                        if let Err(e) = backend.query(&q, k).await {
                            report_crash(
                                config,
                                progress,
                                &format!("search '{}' warmup query #{qi}", query.name),
                                &e,
                            );
                            return Err(e);
                        }
                    }
                }
                CacheMode::Cold | CacheMode::DiskCold => {
                    // Evict shared buffers before measurement.
                    // Skip warmup to measure cold-cache performance.
                    let mut pg_evict = crate::backend::postgres::PgConnection::new(
                        &config.connection,
                        &config.table_name(),
                        config.dataset.dimension,
                        config.dataset.metric,
                    );
                    pg_evict.set_physical_index(idx_config.physical_index.clone());
                    pg_evict.connect().await?;
                    let idx_sql = pg_evict.index_sql_name(&query.index);
                    pg_evict
                        .evict_relation_buffers(&config.table_name())
                        .await?;
                    pg_evict.evict_relation_buffers(&idx_sql).await?;

                    if config.search.cache == CacheMode::DiskCold {
                        // Shared buffers alone leave the OS page cache warm;
                        // drop it so first reads genuinely hit disk. A denied
                        // command degrades this point to cold — logged and
                        // recorded so reports say what actually happened.
                        match crate::oscache::drop_os_caches(&config.search.cache_drop_command) {
                            Ok(()) => {
                                let _ = progress.send(ProgressEvent::Message {
                                    text: "Evicted shared buffers + dropped \
                                           OS page caches (disk-cold mode)"
                                        .to_string(),
                                });
                            }
                            Err(msg) => {
                                effective_cache = "cold (OS cache drop failed)";
                                tracing::warn!(
                                    "disk-cold requested but OS cache drop failed: {msg}"
                                );
                                let _ = progress.send(ProgressEvent::Message {
                                    text: format!(
                                        "WARNING: OS cache drop failed ({msg}) — \
                                         this point measured with shared buffers \
                                         evicted ONLY"
                                    ),
                                });
                            }
                        }
                    } else {
                        let _ = progress.send(ProgressEvent::Message {
                            text: "Evicted shared buffers (cold mode)".to_string(),
                        });
                    }
                }
            }

            // Open the I/O measurement window: flush the query
            // backend's pending stats first so warmup/eviction I/O
            // stays out of the deltas.
            let io_point = match &io_monitor {
                Some(m) => {
                    crate::backend::search_monitor::force_stats_flush(backend.pg().client()).await;
                    Some(m.begin().await)
                }
                None => None,
            };

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
                    let result_ids = match backend.query(&q, k).await {
                        Ok(ids) => ids,
                        Err(e) => {
                            report_crash(
                                config,
                                progress,
                                &format!("search '{}' query #{qi}", query.name),
                                &e,
                            );
                            return Err(e);
                        }
                    };
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

            // Close the I/O window. The stats cover ALL runs (QPS
            // reports the best run), hence queries = num_queries × runs.
            let io_stats = match (&io_monitor, io_point) {
                (Some(m), Some(p)) => {
                    crate::backend::search_monitor::force_stats_flush(backend.pg().client()).await;
                    Some(m.finish(p, (num_queries * config.search.runs) as u64).await)
                }
                _ => None,
            };

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
                index_name: query.name.clone(),
                index_type: idx_config.index_type.clone(),
                data_type: data_type.clone(),
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
                cache: Some(effective_cache.to_string()),
                io: io_stats,
            };

            if let Some(io) = &result.io {
                let hit = io
                    .shared_hit_ratio()
                    .map(|h| format!("{:.1}%", h * 100.0))
                    .unwrap_or_else(|| "n/a".to_string());
                info!(
                    "  io: {:.1} blks read/query (idx {}, heap {}), \
                     buffer hit {}, disk {}/query, disk busy {:.0}%",
                    io.blks_read_per_query(),
                    io.idx_blks_read,
                    io.heap_blks_read,
                    hit,
                    crate::output::plot::fmt_bytes(io.disk_read_bytes_per_query()),
                    io.disk_busy_ratio().unwrap_or(0.0) * 100.0,
                );
            }

            let _ = progress.send(ProgressEvent::SearchEnd {
                index_name: query.name.clone(),
                recall: result.avg_recall,
                qps: result.qps,
            });

            info!(
                "  {} [{}]: recall={:.4} QPS={:.0}",
                query.name, params_json, result.avg_recall, result.qps
            );

            all_results.push(result);

            // Save incrementally per result-series so progress survives crashes
            save_index_results(
                config,
                &query.name,
                &query.index,
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

/// The sweep's most expensive parameter combination: for each swept key,
/// the numerically largest value across all combos (a non-numeric key, or
/// a tie, keeps whichever combo's value was seen first). Used to warm the
/// widest slice of the index before any timed point runs — see the call
/// site in `run`.
fn broadest_params(param_combos: &[HashMap<String, toml::Value>]) -> HashMap<String, toml::Value> {
    let mut broadest = match param_combos.first() {
        Some(p) => p.clone(),
        None => return HashMap::new(),
    };
    for combo in &param_combos[1..] {
        for (key, value) in combo {
            let is_larger = match (value, broadest.get(key)) {
                (toml::Value::Integer(v), Some(toml::Value::Integer(c))) => v > c,
                (toml::Value::Float(v), Some(toml::Value::Float(c))) => v > c,
                (toml::Value::Integer(v), Some(toml::Value::Float(c))) => (*v as f64) > *c,
                (toml::Value::Float(v), Some(toml::Value::Integer(c))) => *v > (*c as f64),
                _ => false,
            };
            if is_larger {
                broadest.insert(key.clone(), value.clone());
            }
        }
    }
    broadest
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

    #[test]
    fn broadest_params_picks_max_of_single_swept_key() {
        // hnsw-halfvec's real sweep: ef_search alone, ascending.
        let mut params = HashMap::new();
        params.insert(
            "hnsw.ef_search".to_string(),
            vec![10, 20, 40, 80, 120, 200, 400, 800, 1000]
                .into_iter()
                .map(toml::Value::Integer)
                .collect(),
        );
        let combos = expand_sweep_params(&params);
        let broadest = broadest_params(&combos);
        assert_eq!(broadest["hnsw.ef_search"].as_integer().unwrap(), 1000);
    }

    #[test]
    fn broadest_params_maxes_each_key_independently() {
        // Cartesian sweep: the widest combo isn't necessarily one that was
        // ever actually measured together, since each key is maxed on its
        // own — that's fine, it only needs to be at least as expensive as
        // every real point so warmup touches at least as much of the index.
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
        let broadest = broadest_params(&combos);
        assert_eq!(broadest["a"].as_integer().unwrap(), 2);
        assert_eq!(broadest["b"].as_integer().unwrap(), 30);
    }

    #[test]
    fn broadest_params_keeps_non_numeric_key_stable() {
        // mkt-norerank's shape: a fixed boolean alongside a swept int.
        let mut params = HashMap::new();
        params.insert("mkt.rerank".to_string(), vec![toml::Value::Boolean(false)]);
        params.insert(
            "mkt.nprobe".to_string(),
            vec![10, 20, 40, 80, 160]
                .into_iter()
                .map(toml::Value::Integer)
                .collect(),
        );
        let combos = expand_sweep_params(&params);
        let broadest = broadest_params(&combos);
        assert_eq!(broadest["mkt.nprobe"].as_integer().unwrap(), 160);
        assert!(!broadest["mkt.rerank"].as_bool().unwrap());
    }

    #[test]
    fn broadest_params_empty_is_empty() {
        assert!(broadest_params(&[]).is_empty());
    }
}
