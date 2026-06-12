pub mod plot;
pub mod stdout;

use crate::metrics::{IndexBuildResult, ParetoPoint, PgRuntimeInfo, RunSummary, SearchResult};
use crate::progress::ProgressEvent;

pub trait OutputSink: Send {
    fn on_phase_start(&mut self, phase: &str, detail: &str);
    fn on_phase_end(&mut self, phase: &str, elapsed_secs: f64);
    fn on_progress(&mut self, event: &ProgressEvent);
    fn on_pg_runtime(&mut self, info: &PgRuntimeInfo);
    fn on_index_built(&mut self, result: &IndexBuildResult);
    fn on_search_result(&mut self, result: &SearchResult);
    fn on_pareto(&mut self, frontier: &[ParetoPoint]);
    fn on_complete(&mut self, summary: &RunSummary);
}
