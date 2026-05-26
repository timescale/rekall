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

    for idx_config in &config.index {
        if let Some(name) = filter_name {
            if idx_config.name != name {
                continue;
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
                progress.cloned(),
            )
            .await?;

        backend.drop_index().await?;
        let result = backend.build_index().await?;

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
