use crate::backend::postgres::PgConnection;
use crate::backend::{BackendRegistry, DataLoader};
use crate::config::BenchmarkConfig;
use crate::error::{Error, Result};
use crate::metrics::IndexBuildResult;
use crate::progress::ProgressTx;

/// Persist a build result to a per-index log file (`build-{name}.json`) as
/// soon as the build finishes, so the timing/size/workers survive the run and
/// can be recovered when the index is later reused (instead of rebuilt). Used
/// by both the standalone `index` phase and the interleaved `run` path.
pub(crate) fn save_build_result(config: &BenchmarkConfig, result: &IndexBuildResult) {
    let dir = &config.output.results_dir;
    let path = format!("{dir}/build-{}.json", result.index_name);
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(result) {
        let _ = std::fs::write(&path, json);
        tracing::info!("Build result saved to {path}");
    }
}

/// Recover a previously saved build result for `index_name`, if its
/// `build-{name}.json` log exists. Lets the reuse path report build
/// time/size/workers without rebuilding.
pub(crate) fn load_build_result(
    config: &BenchmarkConfig,
    index_name: &str,
) -> Option<IndexBuildResult> {
    let dir = &config.output.results_dir;
    let path = format!("{dir}/build-{index_name}.json");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Normalize the table's `parallel_workers` reloption before an index build.
///
/// PRISM's standard-AM build worker count comes from the planner's heap-size
/// heuristic (`ii_ParallelWorkers`), which derates large builds well below
/// `max_parallel_maintenance_workers`; an explicit table reloption overrides
/// that so the build uses the full worker budget. pgvector HNSW reads the GUC
/// directly and a reloption would only cap it, so we RESET it for any index
/// that doesn't request one — no leakage between builds on the shared table.
///
/// Shared by the standalone `index` phase and the interleaved `run` path so the
/// two can't drift (the interleaved path previously skipped this entirely).
pub(crate) async fn normalize_table_parallel_workers(
    admin: &tokio_postgres::Client,
    table: &str,
    workers: Option<i32>,
) -> Result<()> {
    let sql = match workers {
        Some(n) => format!("ALTER TABLE {table} SET (parallel_workers = {n})"),
        None => format!("ALTER TABLE {table} RESET (parallel_workers)"),
    };
    admin.execute(&sql, &[]).await?;
    Ok(())
}

/// Apply the run's cache policy to the heap before a build, so every
/// index arm builds under the same cache conditions regardless of its
/// position in the run order (an earlier build's heap scans otherwise
/// warm the page cache for later arms). Warm: prewarm the table (buffer
/// pool prefix + OS page-cache prefetch of the remainder). Cold: evict
/// the table's shared buffers. Returns the label recorded on the build
/// result ("prewarmed" / "cold").
pub async fn apply_build_cache_policy(config: &BenchmarkConfig) -> crate::Result<&'static str> {
    let mut pg = PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );
    pg.connect().await?;
    match config.search.cache {
        crate::config::CacheMode::Warm => {
            // OS page cache only: build scans stream the heap; loading a
            // heap prefix into shared buffers would only churn them.
            pg.prewarm_oscache(&config.table_name()).await?;
            Ok("prewarmed")
        }
        crate::config::CacheMode::Cold => {
            pg.evict_relation_buffers(&config.table_name()).await?;
            Ok("cold")
        }
        crate::config::CacheMode::DiskCold => {
            pg.evict_relation_buffers(&config.table_name()).await?;
            match crate::oscache::drop_os_caches(&config.search.cache_drop_command) {
                Ok(()) => Ok("disk-cold"),
                Err(msg) => {
                    tracing::warn!(
                        "disk-cold requested but OS cache drop failed \
                         before build: {msg} — building with shared \
                         buffers evicted only"
                    );
                    Ok("cold (OS cache drop failed)")
                }
            }
        }
    }
}

pub async fn run(
    config: &BenchmarkConfig,
    registry: &BackendRegistry,
    filter_name: Option<&str>,
    progress: Option<&ProgressTx>,
) -> Result<Vec<IndexBuildResult>> {
    let pg = PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );

    if !pg.table_exists().await? {
        return Err(Error::TableNotFound {
            table: config.table_name(),
        });
    }

    let count = pg.row_count().await?;
    if count == 0 {
        return Err(Error::TableEmpty {
            table: config.table_name(),
        });
    }

    let mut results = Vec::new();

    // Admin connection used to normalize the table's parallel_workers
    // reloption before each build (see IndexConfig::table_parallel_workers).
    let admin = pg.open_connection().await?;

    for idx_config in &config.index {
        if let Some(name) = filter_name {
            if idx_config.name != name {
                continue;
            }
        }

        normalize_table_parallel_workers(
            &admin,
            &config.table_name(),
            idx_config.table_parallel_workers,
        )
        .await?;

        let backend = registry
            .create(
                &config.connection,
                &config.table_name(),
                config.dataset.dimension,
                config.dataset.metric,
                idx_config,
                None,
                crate::config::QueryMode::Default,
                progress.cloned(),
            )
            .await?;

        backend.drop_index().await?;
        let cache_label = apply_build_cache_policy(config).await?;
        let result = match backend.build_index().await {
            Ok(r) => r,
            Err(e) => {
                if let Some(path) = crate::diagnostics::report_if_crash(
                    &config.output.results_dir,
                    &config.connection.dsn,
                    &format!("build index '{}'", idx_config.name),
                    &e,
                ) {
                    tracing::error!(
                        "crash/disconnect building '{}'; diagnostics: {}",
                        idx_config.name,
                        path.display()
                    );
                    if let Some(tx) = progress {
                        let _ = tx.send(crate::progress::ProgressEvent::Message {
                            text: format!(
                                "crash/disconnect building '{}' — diagnostics written to {}",
                                idx_config.name,
                                path.display()
                            ),
                        });
                    }
                }
                return Err(e);
            }
        };

        let mut result = result;
        result.cache = Some(cache_label.to_string());
        save_build_result(config, &result);
        results.push(result);
    }

    if let Some(name) = filter_name {
        if results.is_empty() {
            return Err(Error::IndexConfigNotFound {
                name: name.to_string(),
            });
        }
    }

    Ok(results)
}
