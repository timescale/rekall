// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::Instant;

use ndarray::Array2;
use tracing::info;

use crate::backend::search_monitor::{force_stats_flush, SearchIoMonitor};
use crate::backend::{BackendRegistry, IndexBackend};
use crate::config::{
    BenchmarkConfig, CacheMode, ClientCount, ClientLevel, IndexConfig, QueryConfig,
};
use crate::dataset::hdf5::Hdf5Reader;
use crate::dataset::DatasetReader;
use crate::error::{Error, Result};
use crate::metrics::{
    compute_recall, latest_concurrency_results, mean_recall_distance_based, recall_distance_based,
    ConcurrencyResult, IndexBuildResult, LatencyCollector, PgRuntimeInfo, QueryResult, RecallBasis,
    RunEnvironment, RunSummary, SearchResult, DISTANCE_RECALL_EPSILON,
};
use crate::phase::concurrency::{
    free_connection_slots, queries_per_run, run_timed, select_operating_points,
    split_levels_by_fit, warm_up, ClientFailure, QueryFn, RecallFn, SESSION_WARMUP_QUERIES,
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

/// Sweep and concurrency results, as saved in a results directory or
/// returned by the search phase.
#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub search: Vec<SearchResult>,
    /// One copy of each measurement, the newest.
    pub concurrency: Vec<ConcurrencyResult>,
}

impl SearchResults {
    pub fn extend(&mut self, other: SearchResults) {
        self.search.extend(other.search);
        self.concurrency.extend(other.concurrency);
    }
}

/// Load previously completed results from per-index result files in results_dir.
/// Scans for {index_name}-*.json and {index_name}-*.json.gz files.
pub fn load_existing_results(config: &BenchmarkConfig) -> SearchResults {
    let dir = &config.output.results_dir;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return SearchResults::default();
    };

    let mut search = Vec::new();
    let mut concurrency = Vec::new();

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
        search.extend(summary.search_results);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        concurrency.push((name, summary.concurrency_results));
    }

    SearchResults {
        search,
        concurrency: latest_concurrency_results(concurrency),
    }
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
        concurrency_results: Vec::new(),
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
) -> Result<SearchResults> {
    let dataset_path = config.dataset.resolved_path();
    let reader = Hdf5Reader::open(
        &dataset_path,
        config.dataset.metric,
        config.dataset.dimension,
    )?;

    // Shared with the concurrent clients' tasks.
    let queries = Arc::new(reader.queries()?);
    let ground_truth = Arc::new(reader.ground_truth()?);
    let gt_distances = reader.ground_truth_distances()?.map(Arc::new);

    let num_queries = config
        .search
        .num_queries
        .unwrap_or(queries.nrows())
        .min(queries.nrows());

    let k = config.search.k;

    save_ground_truth(
        config,
        &ground_truth,
        gt_distances.as_deref(),
        num_queries,
        k,
    );

    // With --rerun, ignore previously-saved results so every sweep point is
    // re-measured. Otherwise the run is resumable: completed points are
    // loaded and skipped (see `is_completed`).
    let existing = if rerun {
        SearchResults::default()
    } else {
        load_existing_results(config)
    };
    let mut all_results = existing.search;
    let existing_concurrency = existing.concurrency;

    // Series this call will actually (re)write. `save_index_results` rewrites
    // a series' whole file from `all_results` each time one of its points
    // finishes, so per-query neighbor IDs (`query_results`) must stay resident
    // for those series. Every other series already sits complete and correct
    // on disk from a prior run/call and is only ever read here for
    // `is_completed` (index_name + params, not query_results) — so drop its
    // heavy payload immediately instead of holding the whole sweep's history
    // (every index × every query × k neighbors) in memory for the duration of
    // this call.
    let effective_queries = config.effective_queries();
    let active_series: std::collections::HashSet<&str> = effective_queries
        .iter()
        .filter(|q| filter_index.is_none_or(|name| q.index == name))
        .map(|q| q.name.as_str())
        .collect();
    for r in &mut all_results {
        if !active_series.contains(r.index_name.as_str()) {
            r.query_results = Vec::new();
        }
    }
    // Returned for the series this call searches: what was already on disk
    // plus what it measures.
    let mut concurrency_results: Vec<ConcurrencyResult> = existing_concurrency
        .iter()
        .filter(|r| active_series.contains(r.index_name.as_str()))
        .cloned()
        .collect();

    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();

    // Capture the PG runtime now, at search time, rather than trusting the
    // snapshot threaded in from run start. By now every index has been built,
    // so all extensions under test exist in the catalog — meaning each
    // per-index file records the correct extension version + git commit +
    // settings. The run-start snapshot predates `CREATE EXTENSION` in the
    // index phase, so it omits e.g. PRISM entirely. In the interleaved
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

    let cpu_count = crate::environment::collect_cpu_count();
    let concurrency_levels = if num_queries > 0 {
        concurrency_levels_that_fit(config, progress, cpu_count).await
    } else {
        Vec::new()
    };

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

        let backend = create_series_backend(config, registry, idx_config, query).await?;

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
        let backend_pid = if config.search.io_stats {
            backend_pids(std::slice::from_ref(&backend)).await
        } else {
            Vec::new()
        };
        let io_monitor = if config.search.io_stats {
            let m = SearchIoMonitor::new(
                &config.connection.dsn,
                &active_idx,
                &config.table_name(),
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
            warm_backend(
                config,
                progress,
                backend.as_ref(),
                &queries,
                config.search.warmup.min(num_queries),
                &format!("search '{}' broad warmup", query.name),
            )
            .await?;
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

            // Warmup queries fill shared buffers; cold modes skip them and
            // evict instead.
            let effective_cache = match config.search.cache {
                CacheMode::Warm => {
                    warm_backend(
                        config,
                        progress,
                        backend.as_ref(),
                        &queries,
                        config.search.warmup.min(num_queries),
                        &format!("search '{}' warmup", query.name),
                    )
                    .await?;
                    "warm"
                }
                CacheMode::Cold | CacheMode::DiskCold => {
                    evict_for_cold_run(config, progress, idx_config, &query.index).await?
                }
            };

            // Open the I/O measurement window: flush the query
            // backend's pending stats first so warmup/eviction I/O
            // stays out of the deltas.
            let io_point = match &io_monitor {
                Some(m) => {
                    force_stats_flush(backend.pg().client()).await;
                    Some(m.begin(&backend_pid).await)
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
                    force_stats_flush(backend.pg().client()).await;
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

        if !concurrency_levels.is_empty() {
            let series = ConcurrencySeries {
                config,
                registry,
                progress,
                query,
                idx_config,
                data_type: data_type.clone(),
                queries: queries.clone(),
                recall: SeriesRecall {
                    neighbors: ground_truth.clone(),
                    distances: gt_distances.clone(),
                    k,
                },
                num_queries,
                levels: &concurrency_levels,
                cpu_count,
                io_monitor: io_monitor.as_ref(),
                timestamp: &timestamp,
                environment,
                pg_runtime,
            };
            let current = measure_series_concurrency(
                &series,
                backend,
                &param_combos,
                &all_results,
                &existing_concurrency,
            )
            .await?;
            concurrency_results.retain(|r| r.index_name != query.name);
            concurrency_results.extend(current);
        }

        // This series is done and fully durable in its own file (each point
        // above rewrote it with the complete accumulated set). Nothing else
        // in this call touches `query.name` again, so drop its per-query
        // neighbor IDs from memory now rather than carrying them for the
        // rest of the sweep.
        for r in all_results
            .iter_mut()
            .filter(|r| r.index_name == query.name)
        {
            r.query_results = Vec::new();
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

    Ok(SearchResults {
        search: all_results,
        concurrency: concurrency_results,
    })
}

/// Open a search connection for a query series, set up like every other
/// connection measuring it: same backend, query mode and physical index.
async fn create_series_backend(
    config: &BenchmarkConfig,
    registry: &BackendRegistry,
    idx_config: &IndexConfig,
    query: &QueryConfig,
) -> Result<Box<dyn IndexBackend>> {
    registry
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
        .await
}

/// Run the first `count` queries untimed on `backend` to fill the caches.
async fn warm_backend(
    config: &BenchmarkConfig,
    progress: &ProgressTx,
    backend: &dyn IndexBackend,
    queries: &Array2<f32>,
    count: usize,
    context: &str,
) -> Result<()> {
    for qi in 0..count {
        let q = queries.row(qi).to_vec();
        if let Err(e) = backend.query(&q, config.search.k).await {
            report_crash(config, progress, &format!("{context} query #{qi}"), &e);
            return Err(e);
        }
    }
    Ok(())
}

/// Evict the table's and index's shared buffers, and in disk-cold mode the
/// OS page cache too. Returns the cache state reached, which is plain cold
/// when the OS cache drop is denied.
async fn evict_for_cold_run(
    config: &BenchmarkConfig,
    progress: &ProgressTx,
    idx_config: &IndexConfig,
    index_name: &str,
) -> Result<&'static str> {
    let mut pg_evict = crate::backend::postgres::PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );
    pg_evict.set_physical_index(idx_config.physical_index.clone());
    pg_evict.connect().await?;
    let idx_sql = pg_evict.index_sql_name(index_name);
    pg_evict
        .evict_relation_buffers(&config.table_name())
        .await?;
    pg_evict.evict_relation_buffers(&idx_sql).await?;

    if config.search.cache != CacheMode::DiskCold {
        let _ = progress.send(ProgressEvent::Message {
            text: "Evicted shared buffers (cold mode)".to_string(),
        });
        return Ok("cold");
    }
    // Shared buffers alone leave the OS page cache warm; drop it so first
    // reads genuinely hit disk. A denied command degrades this point to
    // cold, logged and recorded so reports say what actually happened.
    match crate::oscache::drop_os_caches(&config.search.cache_drop_command) {
        Ok(()) => {
            let _ = progress.send(ProgressEvent::Message {
                text: "Evicted shared buffers + dropped OS page caches (disk-cold mode)"
                    .to_string(),
            });
            Ok("disk-cold")
        }
        Err(msg) => {
            tracing::warn!("disk-cold requested but OS cache drop failed: {msg}");
            let _ = progress.send(ProgressEvent::Message {
                text: format!(
                    "WARNING: OS cache drop failed ({msg}) — this point measured \
                     with shared buffers evicted ONLY"
                ),
            });
            Ok("cold (OS cache drop failed)")
        }
    }
}

/// PostgreSQL backend pids of the clients' connections, for per-backend I/O.
async fn backend_pids(clients: &[Box<dyn IndexBackend>]) -> Vec<i32> {
    let mut pids = Vec::with_capacity(clients.len());
    for client in clients {
        if let Ok(row) = client
            .pg()
            .client()
            .query_one("SELECT pg_backend_pid()", &[])
            .await
        {
            pids.push(row.get::<_, i32>(0));
        }
    }
    pids
}

/// Flush every client's pending I/O stats, all at once since each flush
/// may fall back to a one-second settle sleep.
async fn flush_stats(clients: &[Box<dyn IndexBackend>]) {
    futures_util::future::join_all(clients.iter().map(|c| force_stats_flush(c.pg().client())))
        .await;
}

/// The `search.clients` levels on this host whose connections fit in
/// PostgreSQL's free connection slots. Levels that don't fit are skipped
/// with a warning. If the slots can't be read, every level is kept.
async fn concurrency_levels_that_fit(
    config: &BenchmarkConfig,
    progress: &ProgressTx,
    cpu_count: usize,
) -> Vec<ClientLevel> {
    let levels = config.search.concurrency_levels(cpu_count);
    if levels.is_empty() {
        return levels;
    }
    let available = match free_slots(config).await {
        Ok(n) => n,
        Err(e) => {
            // A connection error's own text carries the DSN, password and all.
            let reason = match e {
                Error::PgConnect { source, .. } => source.to_string(),
                other => other.to_string(),
            };
            tracing::warn!(
                "could not read PostgreSQL's free connection slots ({reason}); \
                 measuring every search.clients level"
            );
            return levels;
        }
    };
    let (fit, skipped) = split_levels_by_fit(&levels, available);
    for (n, spec) in skipped {
        let text = format!(
            "Skipping {n} concurrent clients (search.clients \"{spec}\"): they need \
             {} connections with rekall's own, but PostgreSQL has {available} free. \
             Raise max_connections to measure them.",
            n.saturating_add(crate::phase::concurrency::OWN_CONNECTIONS)
        );
        tracing::warn!("{text}");
        let _ = progress.send(ProgressEvent::Message { text });
    }
    fit
}

/// Free connection slots on the server. Counts its own probe connection as
/// open, so it errs one slot low.
async fn free_slots(config: &BenchmarkConfig) -> Result<usize> {
    let mut pg = crate::backend::postgres::PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );
    pg.connect().await?;
    let row = pg
        .client()
        .query_one(
            "SELECT current_setting('max_connections')::int8, \
                    current_setting('superuser_reserved_connections')::int8 \
                      + COALESCE(current_setting('reserved_connections', true)::int8, 0), \
                    (SELECT rolsuper FROM pg_roles WHERE rolname = current_user), \
                    (SELECT count(*) FROM pg_stat_activity \
                      WHERE backend_type = 'client backend')",
            &[],
        )
        .await?;
    Ok(free_connection_slots(
        row.get(0),
        row.get(1),
        row.get(2),
        row.get(3),
    ))
}

/// What measuring a query series under concurrency needs from the search
/// phase.
struct ConcurrencySeries<'a> {
    config: &'a BenchmarkConfig,
    registry: &'a BackendRegistry,
    progress: &'a ProgressTx,
    query: &'a QueryConfig,
    idx_config: &'a IndexConfig,
    data_type: Option<String>,
    queries: Arc<Array2<f32>>,
    recall: SeriesRecall,
    num_queries: usize,
    levels: &'a [ClientLevel],
    cpu_count: usize,
    io_monitor: Option<&'a SearchIoMonitor>,
    timestamp: &'a str,
    environment: Option<&'a RunEnvironment>,
    pg_runtime: Option<&'a PgRuntimeInfo>,
}

/// A sweep point chosen for concurrent measurement.
struct OperatingPoint<'a> {
    params: &'a HashMap<String, toml::Value>,
    params_json: serde_json::Value,
    recall_targets: Vec<f64>,
    /// The sweep's recall here on the series' basis, which the concurrent
    /// runs should reproduce. `None` when the sweep kept no per-query
    /// results to count it from.
    sweep_recall: Option<f64>,
}

/// Counts recall for a series' concurrency measurement on one basis:
/// distance-based when the dataset has ground-truth distances, as in the
/// published reports, else by neighbour ID.
#[derive(Clone)]
struct SeriesRecall {
    neighbors: Arc<Array2<i64>>,
    distances: Option<Arc<Array2<f32>>>,
    k: usize,
}

impl SeriesRecall {
    fn basis(&self) -> RecallBasis {
        match self.distances {
            Some(_) => RecallBasis::Distance,
            None => RecallBasis::Id,
        }
    }

    /// Recall of the IDs returned for query-set entry `qi`.
    fn of_query(&self, qi: usize, ids: &[i64]) -> f64 {
        let neighbors = self.neighbors.row(qi).to_vec();
        match &self.distances {
            Some(d) => recall_distance_based(
                ids,
                &neighbors,
                &d.row(qi).to_vec(),
                self.k,
                DISTANCE_RECALL_EPSILON,
            ),
            None => compute_recall(ids, &neighbors, self.k),
        }
    }

    /// A sweep point's mean recall, counted from its per-query results.
    /// `None` when it kept none.
    fn of_sweep_point(&self, result: &SearchResult) -> Option<f64> {
        let Some(distances) = &self.distances else {
            return Some(result.avg_recall);
        };
        let (neighbors, n_cols) = (self.neighbors.as_slice()?, self.neighbors.ncols());
        let (distances, d_cols) = (distances.as_slice()?, distances.ncols());
        mean_recall_distance_based(&result.query_results, self.k, |qi| {
            Some((
                neighbors.get(qi * n_cols..(qi + 1) * n_cols)?,
                distances.get(qi * d_cols..(qi + 1) * d_cols)?,
            ))
        })
    }
}

/// Query function running query-set entry `qi` on a search connection.
// The client type is the boxed backend the pool owns, so `&Box` is the
// reference the runner hands out.
#[allow(clippy::borrowed_box)]
fn backend_query_fn(queries: Arc<Array2<f32>>, k: usize) -> QueryFn<Box<dyn IndexBackend>> {
    Arc::new(move |backend: &Box<dyn IndexBackend>, qi: usize| {
        let q = queries.row(qi).to_vec();
        Box::pin(async move { backend.query(&q, k).await })
    })
}

/// Measure a series' operating points under concurrent load, once its
/// single-client sweep is done. Each recall target's operating point gets a
/// single-client baseline, measured on the sweep's own connection, and then
/// each client level. Results already on disk are skipped. Returns the
/// results measured.
async fn measure_series_concurrency(
    series: &ConcurrencySeries<'_>,
    mut backend: Box<dyn IndexBackend>,
    param_combos: &[HashMap<String, toml::Value>],
    all_results: &[SearchResult],
    existing: &[ConcurrencyResult],
) -> Result<Vec<ConcurrencyResult>> {
    let (config, progress, query) = (series.config, series.progress, series.query);
    let k = config.search.k;
    let message = |text: String| {
        info!("{text}");
        let _ = progress.send(ProgressEvent::Message { text });
    };

    let candidates: Vec<(&HashMap<String, toml::Value>, &SearchResult, Option<f64>)> = param_combos
        .iter()
        .filter_map(|combo| {
            let json = serde_json::to_value(combo).unwrap_or_default();
            let sweep = all_results.iter().find(|r| {
                r.index_name == query.name
                    && r.search_params == json
                    && r.k == k
                    && r.num_queries == series.num_queries
            })?;
            let recall = series.recall.of_sweep_point(sweep);
            Some((combo, sweep, recall))
        })
        .collect();
    let selection = select_operating_points(
        &candidates,
        &config.search.concurrency_recall_targets,
        // A point without per-query results falls back to its ID-based
        // recall, which never exceeds the distance-based one, so it can only
        // understate whether the point reaches a target.
        |c| c.2.unwrap_or(c.1.avg_recall),
        |c| c.1.qps,
    );
    for target in &selection.unreached {
        message(format!(
            "No sweep point of '{}' reaches recall {target}; not measuring it under concurrency",
            query.name
        ));
    }

    let query_fn = backend_query_fn(series.queries.clone(), k);
    let recall_fn: RecallFn = {
        let recall = series.recall.clone();
        Arc::new(move |qi, ids| recall.of_query(qi, ids))
    };

    // The series' current set: earlier results for the points still
    // selected, relabelled with the targets they serve now, plus what this
    // call measures. It replaces the series' earlier file as a whole.
    let earlier: Vec<&ConcurrencyResult> = existing
        .iter()
        .filter(|r| r.index_name == query.name)
        .collect();
    let points: Vec<(&serde_json::Value, &[f64])> = selection
        .points
        .iter()
        .map(|p| {
            (
                &candidates[p.candidate].1.search_params,
                p.recall_targets.as_slice(),
            )
        })
        .collect();
    let conditions = MeasurementConditions {
        k,
        num_queries: series.num_queries,
        num_runs: config.search.runs.max(1),
        cache: requested_cache_label(config.search.cache),
    };
    let mut current = carried_over(&earlier, &points, &conditions);
    let unchanged = current.len() == earlier.len()
        && current.iter().all(|c| {
            earlier.iter().any(|e| {
                e.measures(&c.index_name, &c.search_params, c.clients, k, c.num_queries)
                    && e.recall_targets == c.recall_targets
            })
        });
    if !unchanged {
        save_series_concurrency(series, &current);
    }

    let mut pool: Vec<Box<dyn IndexBackend>> = Vec::new();
    // Most clients the server accepted, once it refused one.
    let mut client_cap = usize::MAX;
    for selected in &selection.points {
        let (params, sweep, sweep_recall) = candidates[selected.candidate];
        let point = OperatingPoint {
            params,
            params_json: sweep.search_params.clone(),
            recall_targets: selected.recall_targets.clone(),
            sweep_recall,
        };
        let is_measured = |clients: usize| {
            current.iter().any(|r| {
                r.measures(
                    &query.name,
                    &point.params_json,
                    clients,
                    k,
                    series.num_queries,
                )
            })
        };
        let baseline_missing = !is_measured(1);
        let mut missing_levels: Vec<ClientLevel> = series
            .levels
            .iter()
            .copied()
            .filter(|(n, _)| !is_measured(*n) && *n <= client_cap)
            .collect();
        if !baseline_missing && missing_levels.is_empty() {
            message(format!(
                "Skipping concurrency for '{}' {} (already measured)",
                query.name, point.params_json
            ));
            continue;
        }

        // The baseline's cache preparation also warms the data for the
        // client levels after it.
        backend.set_search_params(&series.idx_config.gucs).await?;
        backend.set_search_params(point.params).await?;
        let cache = match config.search.cache {
            CacheMode::Warm => {
                warm_backend(
                    config,
                    progress,
                    backend.as_ref(),
                    &series.queries,
                    config.search.warmup.min(series.num_queries),
                    &format!("concurrency '{}' warmup", query.name),
                )
                .await?;
                "warm"
            }
            CacheMode::Cold | CacheMode::DiskCold => {
                evict_for_cold_run(config, progress, series.idx_config, &query.index).await?
            }
        };
        if baseline_missing {
            let (mut clients, result) = measure_clients(
                series,
                &point,
                vec![backend],
                None,
                cache,
                &query_fn,
                &recall_fn,
            )
            .await?;
            backend = clients.pop().expect("the baseline client is handed back");
            record_result(series, &mut current, result);
        }

        let Some(&(widest, _)) = missing_levels.last() else {
            continue;
        };
        while pool.len() < widest {
            match open_pool_client(series, pool.len(), widest).await? {
                PoolClient::Opened(client) => pool.push(client),
                PoolClient::OutOfSlots(reason) => {
                    client_cap = pool.len();
                    let skipped: Vec<String> = missing_levels
                        .iter()
                        .filter(|(n, _)| *n > client_cap)
                        .map(|(n, spec)| format!("{n} (\"{spec}\")"))
                        .collect();
                    let text = format!(
                        "Skipping {} concurrent clients for '{}': PostgreSQL refused \
                         connection {} ({reason}). Raise max_connections to measure them.",
                        skipped.join(", "),
                        query.name,
                        client_cap + 1
                    );
                    tracing::warn!("{text}");
                    let _ = progress.send(ProgressEvent::Message { text });
                    missing_levels.retain(|(n, _)| *n <= client_cap);
                    break;
                }
            }
        }
        for client in &pool {
            client.set_search_params(&series.idx_config.gucs).await?;
            client.set_search_params(point.params).await?;
        }
        for (n, spec) in missing_levels {
            let clients: Vec<_> = pool.drain(..n).collect();
            let clients = warm_up(
                clients,
                series.num_queries,
                SESSION_WARMUP_QUERIES,
                query_fn.clone(),
            )
            .await
            .map_err(|f| client_failure(series, f, n))?;
            let cache = match config.search.cache {
                CacheMode::Warm => "warm",
                CacheMode::Cold | CacheMode::DiskCold => {
                    evict_for_cold_run(config, progress, series.idx_config, &query.index).await?
                }
            };
            let (clients, result) = measure_clients(
                series,
                &point,
                clients,
                Some(spec),
                cache,
                &query_fn,
                &recall_fn,
            )
            .await?;
            pool.extend(clients);
            record_result(series, &mut current, result);
        }
    }
    Ok(current)
}

/// What a concurrency measurement must have been taken under to stand for
/// the current config.
struct MeasurementConditions {
    k: usize,
    num_queries: usize,
    num_runs: usize,
    cache: &'static str,
}

/// The cache state a measurement records when the configured mode was
/// applied as asked.
fn requested_cache_label(mode: CacheMode) -> &'static str {
    match mode {
        CacheMode::Warm => "warm",
        CacheMode::Cold => "cold",
        CacheMode::DiskCold => "disk-cold",
    }
}

/// Earlier results that still stand for the series: those at a selected
/// point and taken under the current conditions, relabelled with the
/// targets their point serves now. Anything else is measured again. A
/// disk-cold point that fell back to plain cold is retried.
fn carried_over(
    earlier: &[&ConcurrencyResult],
    points: &[(&serde_json::Value, &[f64])],
    conditions: &MeasurementConditions,
) -> Vec<ConcurrencyResult> {
    let mut current = Vec::new();
    for (params, targets) in points {
        for r in earlier {
            if r.search_params == **params
                && r.k == conditions.k
                && r.num_queries == conditions.num_queries
                && r.num_runs == conditions.num_runs
                && r.cache.as_deref() == Some(conditions.cache)
            {
                let mut r = (*r).clone();
                r.recall_targets = targets.to_vec();
                current.push(r);
            }
        }
    }
    current
}

/// A concurrent client, or the server's refusal when it is out of
/// connection slots.
enum PoolClient {
    Opened(Box<dyn IndexBackend>),
    OutOfSlots(String),
}

/// Open concurrent client `index` of `wanted`. Running out of connection
/// slots is reported rather than failed, so the caller can skip the levels
/// that need more. Other connection errors leave out the DSN, which may hold
/// a password.
// Pool clients connect like the sweep's connection, so a backend with a
// derived column would re-run its DDL per client; none has one today.
async fn open_pool_client(
    series: &ConcurrencySeries<'_>,
    index: usize,
    wanted: usize,
) -> Result<PoolClient> {
    match create_series_backend(
        series.config,
        series.registry,
        series.idx_config,
        series.query,
    )
    .await
    {
        Ok(client) => Ok(PoolClient::Opened(client)),
        Err(Error::PgConnect { source, .. })
            if source.code() == Some(&tokio_postgres::error::SqlState::TOO_MANY_CONNECTIONS) =>
        {
            Ok(PoolClient::OutOfSlots(source.to_string()))
        }
        Err(Error::PgConnect { source, .. }) => Err(Error::Config(format!(
            "search.clients: cannot open concurrent client {} of {wanted}: {source}",
            index + 1
        ))),
        Err(other) => Err(other),
    }
}

/// Report a failed client query like any other search crash, and return
/// its error.
fn client_failure(series: &ConcurrencySeries<'_>, failure: ClientFailure, clients: usize) -> Error {
    report_crash(
        series.config,
        series.progress,
        &format!(
            "concurrency '{}' with {clients} clients query #{}",
            series.query.name, failure.query_index
        ),
        &failure.error,
    );
    failure.error
}

/// Time `clients` at an operating point over `search.runs` runs, keeping
/// the run with the highest wall-clock QPS, the figure reports lead with.
/// Hands the clients back.
async fn measure_clients(
    series: &ConcurrencySeries<'_>,
    point: &OperatingPoint<'_>,
    mut clients: Vec<Box<dyn IndexBackend>>,
    spec: Option<ClientCount>,
    cache: &str,
    query_fn: &QueryFn<Box<dyn IndexBackend>>,
    recall_fn: &RecallFn,
) -> Result<(Vec<Box<dyn IndexBackend>>, ConcurrencyResult)> {
    let config = series.config;
    let n = clients.len();
    let total = queries_per_run(series.num_queries, n);
    let runs = config.search.runs.max(1);

    let io_point = match series.io_monitor {
        Some(monitor) => {
            let pids = backend_pids(&clients).await;
            flush_stats(&clients).await;
            Some(monitor.begin(&pids).await)
        }
        None => None,
    };

    let mut best: Option<(
        crate::phase::concurrency::Throughput,
        LatencyCollector,
        f64,
        f64,
    )> = None;
    for _ in 0..runs {
        let run = run_timed(
            clients,
            series.num_queries,
            total,
            query_fn.clone(),
            recall_fn.clone(),
        )
        .await
        .map_err(|f| client_failure(series, f, n))?;
        clients = run.clients;
        if best
            .as_ref()
            .is_none_or(|b| run.throughput.qps_wall > b.0.qps_wall)
        {
            best = Some((run.throughput, run.latencies, run.avg_recall, run.cpu_secs));
        }
    }

    // The stats cover every run, hence total × runs queries.
    let io = match (series.io_monitor, io_point) {
        (Some(monitor), Some(p)) => {
            flush_stats(&clients).await;
            Some(monitor.finish(p, total * runs as u64).await)
        }
        _ => None,
    };

    let (throughput, latencies, avg_recall, client_cpu_secs) =
        best.expect("search.runs is at least one");
    let result = ConcurrencyResult {
        index_name: series.query.name.clone(),
        index_type: series.idx_config.index_type.clone(),
        data_type: series.data_type.clone(),
        recall_targets: point.recall_targets.clone(),
        search_params: point.params_json.clone(),
        clients: n,
        clients_spec: spec.map(|s| s.to_string()),
        cpu_count: series.cpu_count,
        k: config.search.k,
        num_queries: series.num_queries,
        num_runs: runs,
        avg_recall,
        recall_basis: series.recall.basis(),
        throughput_qps: throughput.throughput_qps,
        qps_wall: throughput.qps_wall,
        wall_time_till_first_empty_secs: throughput.wall_time_till_first_empty_secs,
        wall_time_total_secs: throughput.wall_time_total_secs,
        queries_finished_till_first_empty: throughput.queries_finished_till_first_empty,
        queries_finished_total: throughput.queries_finished_total,
        avg_latency_us: latencies.mean_us(),
        p50_latency_us: latencies.percentile_us(50.0),
        p95_latency_us: latencies.percentile_us(95.0),
        p99_latency_us: latencies.percentile_us(99.0),
        p999_latency_us: latencies.percentile_us(99.9),
        client_cpu_secs,
        cache: Some(cache.to_string()),
        io,
    };
    if let Some(sweep_recall) = point.sweep_recall {
        if (result.avg_recall - sweep_recall).abs() > 0.005 {
            let text = format!(
                "WARNING: '{}' {} with {n} clients returned recall {:.4}, but the \
                 single-client sweep measured {sweep_recall:.4} at the same params",
                result.index_name, result.search_params, result.avg_recall
            );
            tracing::warn!("{text}");
            let _ = series.progress.send(ProgressEvent::Message { text });
        }
    }
    Ok((clients, result))
}

/// Add a measurement to the series' current set, save the set, and report
/// the measurement with its scaling over the single-client baseline.
fn record_result(
    series: &ConcurrencySeries<'_>,
    current: &mut Vec<ConcurrencyResult>,
    result: ConcurrencyResult,
) {
    let baseline_qps = current
        .iter()
        .find(|r| {
            r.measures(
                &result.index_name,
                &result.search_params,
                1,
                result.k,
                result.num_queries,
            )
        })
        .map(|r| r.qps_wall);
    let scaling = match baseline_qps {
        Some(base) if result.clients > 1 && base > 0.0 => {
            format!(" (x{:.2} over 1 client)", result.qps_wall / base)
        }
        _ => String::new(),
    };
    let text = format!(
        "Concurrency '{}' {} with {} clients: {:.0} QPS{scaling}, p99 {:.0} us",
        result.index_name,
        result.search_params,
        result.clients,
        result.qps_wall,
        result.p99_latency_us
    );
    info!("{text}");
    let _ = series.progress.send(ProgressEvent::Message { text });

    current.push(result);
    save_series_concurrency(series, current);
}

/// Write the series' current concurrency set as its newest concurrency
/// file, which the loaders take as the whole set.
fn save_series_concurrency(series: &ConcurrencySeries<'_>, current: &[ConcurrencyResult]) {
    let summary = RunSummary {
        dataset: series.config.dataset.name.clone(),
        environment: series.environment.cloned(),
        config: serde_json::to_value(series.config).unwrap_or(serde_json::Value::Null),
        pg_runtime: series.pg_runtime.cloned(),
        index_builds: Vec::new(),
        search_results: Vec::new(),
        concurrency_results: current.to_vec(),
        pareto: Vec::new(),
    };
    let path = series
        .config
        .output
        .concurrency_results_path(&series.query.name, series.timestamp);
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match write_json_gz(&path, &summary) {
        Ok(()) => info!("Concurrency results saved to {path}"),
        Err(e) => tracing::error!("Failed to save concurrency results to {path}: {e}"),
    }
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
    fn carried_over_keeps_only_selected_points_measured_the_same_way() {
        use crate::metrics::test_concurrency_result as result;
        let with = |nprobe: i64, runs: usize, cache: &str| {
            let mut r = result("a", nprobe, 16, 100.0);
            r.num_runs = runs;
            r.cache = Some(cache.to_string());
            r
        };
        let kept = with(10, 3, "warm");
        let earlier_results = [
            kept.clone(),
            // A point the selection no longer picks.
            with(20, 3, "warm"),
            // The selected point, but best of 1 or measured cold.
            with(10, 1, "warm"),
            with(10, 3, "cold"),
        ];
        let earlier: Vec<&ConcurrencyResult> = earlier_results.iter().collect();
        let params = kept.search_params.clone();
        let conditions = MeasurementConditions {
            k: kept.k,
            num_queries: kept.num_queries,
            num_runs: 3,
            cache: "warm",
        };
        let current = carried_over(&earlier, &[(&params, &[0.99][..])], &conditions);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].search_params, params);
        assert_eq!(current[0].num_runs, 3);
        // Relabelled with the target its point serves now.
        assert_eq!(current[0].recall_targets, vec![0.99]);
    }

    #[test]
    fn load_existing_results_returns_deduped_concurrency_results() {
        let dir = tempfile::tempdir().unwrap();
        let config: BenchmarkConfig = toml::from_str(&format!(
            r#"
[dataset]
name = "d"
metric = "angular"
dimension = 3

[output]
results_dir = "{}"
"#,
            dir.path().display()
        ))
        .unwrap();
        let concurrency = |clients: usize, qps: f64| {
            crate::metrics::test_concurrency_result("hnsw", 40, clients, qps)
        };
        let write = |name: &str, results: Vec<ConcurrencyResult>| {
            let summary = RunSummary {
                dataset: "d".to_string(),
                environment: None,
                config: serde_json::Value::Null,
                pg_runtime: None,
                index_builds: Vec::new(),
                search_results: Vec::new(),
                concurrency_results: results,
                pareto: Vec::new(),
            };
            write_json_gz(dir.path().join(name).to_str().unwrap(), &summary).unwrap();
        };
        write(
            "hnsw-concurrency-20260610-000000.json.gz",
            vec![concurrency(16, 100.0)],
        );
        write(
            "hnsw-concurrency-20260611-000000.json.gz",
            vec![concurrency(16, 200.0), concurrency(32, 300.0)],
        );

        let existing = load_existing_results(&config);
        assert!(existing.search.is_empty());
        let got: Vec<(usize, f64)> = existing
            .concurrency
            .iter()
            .map(|r| (r.clients, r.throughput_qps))
            .collect();
        assert_eq!(got, vec![(16, 200.0), (32, 300.0)]);
    }

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
            "prism.nprobe".to_string(),
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
            .map(|c| c["prism.nprobe"].as_integer().unwrap())
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
        // prism-norerank's shape: a fixed boolean alongside a swept int.
        let mut params = HashMap::new();
        params.insert(
            "prism.rerank".to_string(),
            vec![toml::Value::Boolean(false)],
        );
        params.insert(
            "prism.nprobe".to_string(),
            vec![10, 20, 40, 80, 160]
                .into_iter()
                .map(toml::Value::Integer)
                .collect(),
        );
        let combos = expand_sweep_params(&params);
        let broadest = broadest_params(&combos);
        assert_eq!(broadest["prism.nprobe"].as_integer().unwrap(), 160);
        assert!(!broadest["prism.rerank"].as_bool().unwrap());
    }

    #[test]
    fn broadest_params_empty_is_empty() {
        assert!(broadest_params(&[]).is_empty());
    }
}
