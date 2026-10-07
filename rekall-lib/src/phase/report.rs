use crate::metrics::{
    compute_pareto, IndexBuildResult, PgRuntimeInfo, RunEnvironment, RunSummary, SearchResult,
};
use crate::output::OutputSink;
use crate::phase::search::SearchResults;

pub fn run(
    dataset: &str,
    environment: Option<RunEnvironment>,
    config: serde_json::Value,
    pg_runtime: Option<PgRuntimeInfo>,
    index_builds: Vec<IndexBuildResult>,
    results: SearchResults,
    outputs: &mut [Box<dyn OutputSink>],
) {
    let pareto = compute_pareto(&results.search);

    let summary_results: Vec<SearchResult> = results
        .search
        .into_iter()
        .map(|mut r| {
            r.query_results = Vec::new();
            r
        })
        .collect();

    let summary = RunSummary {
        dataset: dataset.to_string(),
        environment,
        config,
        pg_runtime,
        index_builds,
        search_results: summary_results,
        concurrency_results: results.concurrency,
        pareto: pareto.clone(),
    };

    for output in outputs.iter_mut() {
        output.on_pareto(&pareto);
        output.on_complete(&summary);
    }
}
