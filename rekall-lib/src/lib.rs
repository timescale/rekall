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

use config::BenchmarkConfig;
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

    pub async fn run(&mut self, phases: &[Phase], filter_index: Option<&str>) -> Result<()> {
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
            self.run_index_search_interleaved(filter_index, &mut index_builds, &mut search_results)
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

    /// Build each index, run its search sweeps, then drop it before
    /// building the next. This allows multiple configs of the same index
    /// type to be explored in a single run.
    async fn run_index_search_interleaved(
        &mut self,
        filter_index: Option<&str>,
        index_builds: &mut Vec<IndexBuildResult>,
        search_results: &mut Vec<SearchResult>,
    ) -> Result<()> {
        for idx_config in &self.config.index.clone() {
            if let Some(name) = filter_index {
                if idx_config.name != name {
                    continue;
                }
            }

            // Build
            let idx_start = Instant::now();
            for output in self.outputs.iter_mut() {
                output.on_phase_start("index", &idx_config.name);
            }

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

            backend.drop_index().await?;
            let build_result = backend.build_index().await?;
            for output in self.outputs.iter_mut() {
                output.on_index_built(&build_result);
            }
            index_builds.push(build_result);

            let idx_elapsed = idx_start.elapsed().as_secs_f64();
            for output in self.outputs.iter_mut() {
                output.on_phase_end("index", idx_elapsed);
            }

            // Search sweeps for this index
            let sweeps: Vec<_> = self
                .config
                .search
                .sweep
                .iter()
                .filter(|s| s.index == idx_config.name)
                .cloned()
                .collect();

            if !sweeps.is_empty() {
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

            // Drop index to make room for the next config
            let _ = backend.drop_index().await;

            let _ = &self.outputs.iter_mut().for_each(|o| {
                o.on_progress(&ProgressEvent::Message {
                    text: format!("Dropped index '{}' for next config", idx_config.name),
                });
            });
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
