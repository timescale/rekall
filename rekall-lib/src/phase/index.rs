use crate::backend::postgres::PgConnection;
use crate::backend::{BackendRegistry, DataLoader};
use crate::config::BenchmarkConfig;
use crate::error::{Error, Result};
use crate::metrics::IndexBuildResult;
use crate::progress::ProgressTx;

fn save_build_result(config: &BenchmarkConfig, result: &IndexBuildResult) {
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

        // Meerkat's standard-AM build is derated by the planner's heap-size
        // heuristic, so set an explicit reloption to let it use the full
        // max_parallel_maintenance_workers. pgvector HNSW reads that GUC
        // directly and a reloption would only cap it, so reset it for any
        // index that doesn't request one — no leakage between builds.
        match idx_config.table_parallel_workers {
            Some(n) => {
                admin
                    .execute(
                        &format!(
                            "ALTER TABLE {} SET (parallel_workers = {n})",
                            config.table_name()
                        ),
                        &[],
                    )
                    .await?;
            }
            None => {
                admin
                    .execute(
                        &format!(
                            "ALTER TABLE {} RESET (parallel_workers)",
                            config.table_name()
                        ),
                        &[],
                    )
                    .await?;
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
                crate::config::QueryMode::Default,
                progress.cloned(),
            )
            .await?;

        backend.drop_index().await?;
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
