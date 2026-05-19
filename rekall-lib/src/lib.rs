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
use progress::progress_channel;

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

    /// Capture PG runtime info by connecting to the database.
    async fn capture_pg_runtime(&self) -> Option<PgRuntimeInfo> {
        let mut pg = backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        );
        // Connect without extension loading — just need basic PG info
        if pg.connect("<=>", None).await.is_err() {
            return None;
        }
        pg.runtime_info().await.ok()
    }

    pub async fn run(&mut self, phases: &[Phase], filter_index: Option<&str>) -> Result<()> {
        let mut index_builds: Vec<IndexBuildResult> = Vec::new();
        let mut search_results: Vec<SearchResult> = Vec::new();
        let mut pg_runtime: Option<PgRuntimeInfo> = None;

        // Capture PG runtime info at the start if any PG-touching phase is requested
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

        for &phase in phases {
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
                    let load_handle =
                        tokio::spawn(async move { phase::load::run(&config, &tx).await });

                    let outputs = &mut self.outputs;
                    while let Some(event) = rx.recv().await {
                        for output in outputs.iter_mut() {
                            output.on_progress(&event);
                        }
                    }

                    load_handle.await.unwrap()?;
                }
                Phase::Index => {
                    let results =
                        phase::index::run(&self.config, &self.registry, filter_index).await?;

                    for result in &results {
                        for output in self.outputs.iter_mut() {
                            output.on_index_built(result);
                        }
                    }
                    index_builds.extend(results);
                }
                Phase::Search => {
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

                    let results = search_handle.await.unwrap()?;
                    for result in &results {
                        for output in self.outputs.iter_mut() {
                            output.on_search_result(result);
                        }
                    }
                    search_results.extend(results);
                }
                Phase::Report => {
                    let config_value = serde_json::to_value(&self.config).unwrap_or_default();
                    phase::report::run(
                        &self.config.dataset.name,
                        config_value,
                        pg_runtime.clone(),
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
        }

        Ok(())
    }
}
