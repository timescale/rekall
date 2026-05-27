#![allow(clippy::collapsible_if)]

pub mod backend;
pub mod config;
pub mod dataset;
pub mod environment;
pub mod error;
pub mod infra;
pub mod metrics;
pub mod output;
pub mod phase;
pub mod progress;

use std::sync::Arc;
use std::time::Instant;

use backend::DataLoader;
use config::{BenchmarkConfig, CacheMode};
use dataset::DatasetReader;
use error::Result;
use metrics::{IndexBuildResult, PgRuntimeInfo, RunEnvironment, SearchResult};
use output::OutputSink;
use phase::Phase;
use progress::{progress_channel, ProgressEvent};
use tokio_postgres::CancelToken;

fn save_run_info(
    config: &BenchmarkConfig,
    env: &RunEnvironment,
    pg_runtime: Option<&PgRuntimeInfo>,
) {
    #[derive(serde::Serialize)]
    struct RunInfo<'a> {
        environment: &'a RunEnvironment,
        pg_runtime: Option<&'a PgRuntimeInfo>,
    }

    let info = RunInfo {
        environment: env,
        pg_runtime,
    };

    let dir = &config.output.results_dir;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = format!("{dir}/run-info-{ts}.json");
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&info) {
        let _ = std::fs::write(&path, json);
        tracing::info!("Run info saved to {path}");
    }
}

/// Shared registry of active PG cancel tokens. On Ctrl+C, all
/// registered tokens are cancelled to stop in-flight PG queries.
pub type CancelRegistry = Arc<tokio::sync::Mutex<Vec<CancelToken>>>;

pub fn new_cancel_registry() -> CancelRegistry {
    Arc::new(tokio::sync::Mutex::new(Vec::new()))
}

/// Spawn a Ctrl+C handler that cancels all registered PG backends.
pub fn install_signal_handler(registry: CancelRegistry) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nInterrupted — cancelling PostgreSQL queries...");
            let tokens = registry.lock().await;
            for token in tokens.iter() {
                let _ = token.cancel_query(tokio_postgres::NoTls).await;
            }
            // Give PG a moment to process cancellations
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            std::process::exit(130);
        }
    });
}

pub struct Agent {
    config: BenchmarkConfig,
    outputs: Vec<Box<dyn OutputSink>>,
    registry: backend::BackendRegistry,
    cancel_registry: CancelRegistry,
}

impl Agent {
    pub fn new(
        config: BenchmarkConfig,
        outputs: Vec<Box<dyn OutputSink>>,
        cancel_registry: CancelRegistry,
    ) -> Self {
        Self {
            config,
            outputs,
            registry: backend::BackendRegistry::new(),
            cancel_registry,
        }
    }

    async fn capture_pg_runtime(&self) -> Option<PgRuntimeInfo> {
        let mut pg = backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        );
        if pg.connect().await.is_err() {
            return None;
        }
        pg.runtime_info().await.ok()
    }

    /// If the dataset file doesn't exist locally and a URI is configured,
    /// download it before running phases that need it.
    async fn ensure_dataset(&mut self, phases: &[Phase]) -> Result<()> {
        let needs_dataset = phases
            .iter()
            .any(|p| matches!(p, Phase::Load | Phase::Search));
        if !needs_dataset {
            return Ok(());
        }

        let uri = match self.config.dataset.uri {
            Some(ref u) => u.clone(),
            None => return Ok(()),
        };

        let dest = self.config.dataset.resolved_path();
        if dest.exists() {
            return Ok(());
        }

        for output in self.outputs.iter_mut() {
            output.on_progress(&ProgressEvent::Message {
                text: format!(
                    "Dataset not found at {}. Downloading from {uri}",
                    dest.display()
                ),
            });
        }

        dataset::download::ensure_available(&uri, &dest).await?;

        for output in self.outputs.iter_mut() {
            output.on_progress(&ProgressEvent::Message {
                text: format!("Dataset ready at {}", dest.display()),
            });
        }

        Ok(())
    }

    pub async fn run(
        &mut self,
        phases: &[Phase],
        filter_index: Option<&str>,
        rebuild_indexes: bool,
        reload_data: bool,
    ) -> Result<()> {
        self.ensure_dataset(phases).await?;

        let run_env = environment::collect();

        let mut index_builds: Vec<IndexBuildResult> = Vec::new();
        let mut search_results: Vec<SearchResult> = Vec::new();
        let mut pg_runtime: Option<PgRuntimeInfo> = None;

        if phases
            .iter()
            .any(|p| matches!(p, Phase::Init | Phase::Load | Phase::Index | Phase::Search))
        {
            pg_runtime = self.capture_pg_runtime().await;
            if let Some(ref info) = pg_runtime {
                for output in self.outputs.iter_mut() {
                    output.on_pg_runtime(info);
                }
            }
        }

        // Write run-info.json early so metadata survives crashes
        save_run_info(&self.config, &run_env, pg_runtime.as_ref());

        // When both Index and Search are requested, interleave them
        // per-index: build → search → drop → next. This allows exploring
        // multiple index configs on the same column in one run.
        let has_index = phases.contains(&Phase::Index);
        let has_search = phases.contains(&Phase::Search);
        let interleave = has_index && has_search;

        // Check if table already has the expected data — skip init/load unless --reload
        let skip_data_phases =
            if !reload_data && (phases.contains(&Phase::Init) || phases.contains(&Phase::Load)) {
                let pg = backend::postgres::PgConnection::new(
                    &self.config.connection,
                    &self.config.table_name(),
                    self.config.dataset.dimension,
                    self.config.dataset.metric,
                );
                if let Ok(true) = pg.table_exists().await {
                    let row_count = pg.row_count().await.unwrap_or(0);
                    if row_count > 0 {
                        // Verify against dataset size
                        let dataset_path = self.config.dataset.resolved_path();
                        if let Ok(reader) = dataset::hdf5::Hdf5Reader::open(
                            &dataset_path,
                            self.config.dataset.metric,
                            self.config.dataset.dimension,
                        ) {
                            let expected = reader.meta().num_vectors;
                            if row_count == expected {
                                true
                            } else {
                                for output in self.outputs.iter_mut() {
                                    output.on_progress(&ProgressEvent::Message {
                                        text: format!(
                                            "Table '{}' has {row_count} rows but dataset has \
                                         {expected} vectors. Use --reload to reload.",
                                            self.config.table_name(),
                                        ),
                                    });
                                }
                                false
                            }
                        } else {
                            true // can't check dataset, assume table is fine
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

        for &phase in phases {
            if interleave && (phase == Phase::Index || phase == Phase::Search) {
                continue;
            }

            if skip_data_phases && (phase == Phase::Init || phase == Phase::Load) {
                let pg = backend::postgres::PgConnection::new(
                    &self.config.connection,
                    &self.config.table_name(),
                    self.config.dataset.dimension,
                    self.config.dataset.metric,
                );
                let count = pg.row_count().await.unwrap_or(0);
                for output in self.outputs.iter_mut() {
                    output.on_progress(&ProgressEvent::Message {
                        text: format!(
                            "Table '{}' already has {count} rows, skipping {phase}",
                            self.config.table_name(),
                        ),
                    });
                }
                continue;
            }

            self.run_phase(
                phase,
                filter_index,
                &mut index_builds,
                &mut search_results,
                &run_env,
                &pg_runtime,
            )
            .await?;
        }

        if interleave {
            self.run_index_search_interleaved(
                filter_index,
                rebuild_indexes,
                &mut index_builds,
                &mut search_results,
                &run_env,
                &pg_runtime,
            )
            .await?;
        }

        // Always run report at the end if it was requested
        if phases.contains(&Phase::Report) && interleave {
            let config_value = serde_json::to_value(&self.config).unwrap_or_default();
            phase::report::run(
                &self.config.dataset.name,
                Some(run_env),
                config_value,
                pg_runtime,
                index_builds,
                search_results,
                &mut self.outputs,
            );
        }

        Ok(())
    }

    async fn run_phase(
        &mut self,
        phase: Phase,
        filter_index: Option<&str>,
        index_builds: &mut Vec<IndexBuildResult>,
        search_results: &mut Vec<SearchResult>,
        run_env: &RunEnvironment,
        pg_runtime: &Option<PgRuntimeInfo>,
    ) -> Result<()> {
        let start = Instant::now();

        for output in self.outputs.iter_mut() {
            output.on_phase_start(&phase.to_string(), "");
        }

        match phase {
            Phase::Init => {
                phase::init::run(&self.config).await?;
            }
            Phase::Load => {
                let (tx, mut rx) = progress_channel();
                let config = self.config.clone();
                let load_handle = tokio::spawn(async move { phase::load::run(&config, &tx).await });

                let outputs = &mut self.outputs;
                while let Some(event) = rx.recv().await {
                    for output in outputs.iter_mut() {
                        output.on_progress(&event);
                    }
                }
                load_handle.await.unwrap()?;
            }
            Phase::Index => {
                let results = phase::index::run(&self.config, &self.registry, filter_index).await?;
                for result in &results {
                    for output in self.outputs.iter_mut() {
                        output.on_index_built(result);
                    }
                }
                index_builds.extend(results);
            }
            Phase::Search => {
                let results = self
                    .run_search(filter_index, Some(run_env.clone()), pg_runtime.clone())
                    .await?;
                for result in &results {
                    for output in self.outputs.iter_mut() {
                        output.on_search_result(result);
                    }
                }
                search_results.extend(results);
            }
            Phase::Report => {
                let pg_runtime = self.capture_pg_runtime().await;
                let config_value = serde_json::to_value(&self.config).unwrap_or_default();
                phase::report::run(
                    &self.config.dataset.name,
                    Some(run_env.clone()),
                    config_value,
                    pg_runtime,
                    index_builds.clone(),
                    search_results.clone(),
                    &mut self.outputs,
                );
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        for output in self.outputs.iter_mut() {
            output.on_phase_end(&phase.to_string(), elapsed);
        }

        Ok(())
    }

    /// Build indexes that don't exist yet, then for each index: disable
    /// all others, run search sweeps, re-enable. Avoids expensive rebuilds
    /// when indexes already exist.
    async fn run_index_search_interleaved(
        &mut self,
        filter_index: Option<&str>,
        rebuild: bool,
        index_builds: &mut Vec<IndexBuildResult>,
        search_results: &mut Vec<SearchResult>,
        run_env: &RunEnvironment,
        pg_runtime: &Option<PgRuntimeInfo>,
    ) -> Result<()> {
        let index_configs: Vec<_> = self
            .config
            .index
            .iter()
            .filter(|idx| {
                if let Some(name) = filter_index {
                    idx.name == name
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        // Phase 1: Build any indexes that don't exist yet
        for idx_config in &index_configs {
            let backend = self
                .registry
                .create(
                    &self.config.connection,
                    &self.config.table_name(),
                    self.config.dataset.dimension,
                    self.config.dataset.metric,
                    idx_config,
                    Some(&self.cancel_registry),
                )
                .await?;

            let exists = backend.index_exists().await?;
            if exists && rebuild {
                backend.drop_index().await?;
                for output in self.outputs.iter_mut() {
                    output.on_progress(&ProgressEvent::Message {
                        text: format!(
                            "Dropping index '{}' for rebuild (--rebuild)",
                            idx_config.name,
                        ),
                    });
                }
            }
            if exists && !rebuild {
                let size = backend.index_size().await.unwrap_or(0);
                let size_mb = size as f64 / (1024.0 * 1024.0);
                for output in self.outputs.iter_mut() {
                    output.on_progress(&ProgressEvent::Message {
                        text: format!(
                            "Index '{}' already exists ({:.1} MB), skipping build",
                            idx_config.name, size_mb,
                        ),
                    });
                }
            } else {
                let idx_start = Instant::now();
                for output in self.outputs.iter_mut() {
                    output.on_phase_start("index", &idx_config.name);
                }

                let build_result = backend.build_index().await?;
                for output in self.outputs.iter_mut() {
                    output.on_index_built(&build_result);
                }
                index_builds.push(build_result);

                let idx_elapsed = idx_start.elapsed().as_secs_f64();
                for output in self.outputs.iter_mut() {
                    output.on_phase_end("index", idx_elapsed);
                }
            }
        }

        // Phase 2: For each index, disable all others, run search, re-enable
        let pg = backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        );
        // Connect to manage index enable/disable
        let mut pg_mgmt = backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        );
        pg_mgmt.connect().await?;

        // Get ALL rekall indexes on the table, not just the ones in this config.
        // Other indexes from previous runs must also be disabled.
        let all_index_names = pg_mgmt.list_rekall_indexes().await?;

        for idx_config in &index_configs {
            let has_sweeps = self
                .config
                .search
                .sweep
                .iter()
                .any(|s| s.index == idx_config.name);
            if !has_sweeps {
                continue;
            }

            let active_idx = pg.index_sql_name(&idx_config.name);

            // Disable all other indexes, enable the active one
            for idx_sql in &all_index_names {
                if idx_sql == &active_idx {
                    pg_mgmt.enable_index(idx_sql).await?;
                } else {
                    pg_mgmt.disable_index(idx_sql).await?;
                }
            }

            // Prewarm in warm mode, evict in cold mode
            if self.config.search.cache == CacheMode::Warm {
                pg_mgmt.prewarm(&self.config.table_name()).await?;
                pg_mgmt.prewarm(&active_idx).await?;
            }

            let search_start = Instant::now();
            for output in self.outputs.iter_mut() {
                output.on_phase_start("search", &idx_config.name);
            }

            let (tx, mut rx) = progress_channel();
            let config = self.config.clone();
            let registry = backend::BackendRegistry::new();
            let idx_name = idx_config.name.clone();

            let env_clone = run_env.clone();
            let pg_clone = pg_runtime.clone();
            let search_handle = tokio::spawn(async move {
                phase::search::run(
                    &config,
                    &registry,
                    &tx,
                    Some(&idx_name),
                    Some(&env_clone),
                    pg_clone.as_ref(),
                )
                .await
            });

            let outputs = &mut self.outputs;
            while let Some(event) = rx.recv().await {
                for output in outputs.iter_mut() {
                    output.on_progress(&event);
                }
            }

            let results = search_handle.await.unwrap()?;
            for result in &results {
                for output in self.outputs.iter_mut() {
                    output.on_search_result(result);
                }
            }
            search_results.extend(results);

            let search_elapsed = search_start.elapsed().as_secs_f64();
            for output in self.outputs.iter_mut() {
                output.on_phase_end("search", search_elapsed);
            }
        }

        // Re-enable all indexes when done
        for idx_sql in &all_index_names {
            let _ = pg_mgmt.enable_index(idx_sql).await;
        }

        Ok(())
    }

    async fn run_search(
        &mut self,
        filter_index: Option<&str>,
        environment: Option<RunEnvironment>,
        pg_runtime: Option<PgRuntimeInfo>,
    ) -> Result<Vec<SearchResult>> {
        let (tx, mut rx) = progress_channel();
        let config = self.config.clone();
        let registry = backend::BackendRegistry::new();
        let filter = filter_index.map(|s| s.to_string());

        let search_handle = tokio::spawn(async move {
            phase::search::run(
                &config,
                &registry,
                &tx,
                filter.as_deref(),
                environment.as_ref(),
                pg_runtime.as_ref(),
            )
            .await
        });

        let outputs = &mut self.outputs;
        while let Some(event) = rx.recv().await {
            for output in outputs.iter_mut() {
                output.on_progress(&event);
            }
        }

        search_handle.await.unwrap()
    }
}
