#![allow(clippy::collapsible_if)]

pub mod backend;
pub mod config;
pub mod dataset;
pub mod error;
pub mod metrics;
pub mod output;
pub mod phase;
pub mod progress;

use std::time::Instant;

use config::{BenchmarkConfig, CacheMode};
use error::Result;
use metrics::{IndexBuildResult, PgRuntimeInfo, SearchResult};
use output::OutputSink;
use phase::Phase;
use progress::{progress_channel, ProgressEvent};

pub struct Agent {
    config: BenchmarkConfig,
    outputs: Vec<Box<dyn OutputSink>>,
    registry: backend::BackendRegistry,
}

impl Agent {
    pub fn new(config: BenchmarkConfig, outputs: Vec<Box<dyn OutputSink>>) -> Self {
        Self {
            config,
            outputs,
            registry: backend::BackendRegistry::new(),
        }
    }

    async fn capture_pg_runtime(&self) -> Option<PgRuntimeInfo> {
        let mut pg = backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        );
        if pg.connect("<=>", None).await.is_err() {
            return None;
        }
        pg.runtime_info().await.ok()
    }

    pub async fn run(
        &mut self,
        phases: &[Phase],
        filter_index: Option<&str>,
        rebuild_indexes: bool,
    ) -> Result<()> {
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

        // When both Index and Search are requested, interleave them
        // per-index: build → search → drop → next. This allows exploring
        // multiple index configs on the same column in one run.
        let has_index = phases.contains(&Phase::Index);
        let has_search = phases.contains(&Phase::Search);
        let interleave = has_index && has_search;

        for &phase in phases {
            // Skip individual Index/Search phases when interleaving
            if interleave && (phase == Phase::Index || phase == Phase::Search) {
                continue;
            }

            self.run_phase(phase, filter_index, &mut index_builds, &mut search_results)
                .await?;
        }

        if interleave {
            self.run_index_search_interleaved(
                filter_index,
                rebuild_indexes,
                &mut index_builds,
                &mut search_results,
            )
            .await?;
        }

        // Always run report at the end if it was requested
        if phases.contains(&Phase::Report) && interleave {
            let config_value = serde_json::to_value(&self.config).unwrap_or_default();
            phase::report::run(
                &self.config.dataset.name,
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
                let results = self.run_search(filter_index).await?;
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
        pg_mgmt.connect("<=>", None).await?;

        let all_index_names: Vec<String> = index_configs
            .iter()
            .map(|c| pg.index_sql_name(&c.name))
            .collect();

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
                pg_mgmt.prewarm(&active_idx).await?;
                pg_mgmt.prewarm(&self.config.table_name()).await?;
            }

            let search_start = Instant::now();
            for output in self.outputs.iter_mut() {
                output.on_phase_start("search", &idx_config.name);
            }

            let (tx, mut rx) = progress_channel();
            let config = self.config.clone();
            let registry = backend::BackendRegistry::new();
            let idx_name = idx_config.name.clone();

            let search_handle = tokio::spawn(async move {
                phase::search::run(&config, &registry, &tx, Some(&idx_name)).await
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

    async fn run_search(&mut self, filter_index: Option<&str>) -> Result<Vec<SearchResult>> {
        let (tx, mut rx) = progress_channel();
        let config = self.config.clone();
        let registry = backend::BackendRegistry::new();
        let filter = filter_index.map(|s| s.to_string());

        let search_handle = tokio::spawn(async move {
            phase::search::run(&config, &registry, &tx, filter.as_deref()).await
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
