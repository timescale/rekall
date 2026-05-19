use std::path::PathBuf;

use crate::metrics::{IndexBuildResult, ParetoPoint, PgRuntimeInfo, RunSummary, SearchResult};
use crate::progress::ProgressEvent;

use super::OutputSink;

pub struct JsonSink {
    path: PathBuf,
}

impl JsonSink {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl OutputSink for JsonSink {
    fn on_phase_start(&mut self, _phase: &str, _detail: &str) {}
    fn on_phase_end(&mut self, _phase: &str, _elapsed_secs: f64) {}
    fn on_progress(&mut self, _event: &ProgressEvent) {}
    fn on_pg_runtime(&mut self, _info: &PgRuntimeInfo) {}
    fn on_index_built(&mut self, _result: &IndexBuildResult) {}
    fn on_search_result(&mut self, _result: &SearchResult) {}
    fn on_pareto(&mut self, _frontier: &[ParetoPoint]) {}

    fn on_complete(&mut self, summary: &RunSummary) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(summary) {
            Ok(json) => match std::fs::write(&self.path, json) {
                Ok(()) => {
                    println!("  Results written to {}", self.path.display());
                }
                Err(e) => {
                    eprintln!("  Failed to write results to {}: {e}", self.path.display());
                }
            },
            Err(e) => {
                eprintln!("  Failed to serialize results: {e}");
            }
        }
    }
}
