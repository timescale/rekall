use crate::metrics::{compute_pareto, IndexBuildResult, PgRuntimeInfo, RunSummary, SearchResult};
use crate::output::OutputSink;

pub fn run(
    dataset: &str,
    config: serde_json::Value,
    pg_runtime: Option<PgRuntimeInfo>,
    index_builds: Vec<IndexBuildResult>,
    search_results: Vec<SearchResult>,
    outputs: &mut [Box<dyn OutputSink>],
) {
    let pareto = compute_pareto(&search_results);

    let summary = RunSummary {
        dataset: dataset.to_string(),
        config,
        pg_runtime,
        index_builds,
        search_results,
        pareto: pareto.clone(),
    };

    for output in outputs.iter_mut() {
        output.on_pareto(&pareto);
        output.on_complete(&summary);
    }
}
