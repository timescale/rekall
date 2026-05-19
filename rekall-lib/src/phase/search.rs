use std::collections::HashMap;
use std::time::Instant;

use tracing::info;

use crate::backend::BackendRegistry;
use crate::config::BenchmarkConfig;
use crate::dataset::DatasetReader;
use crate::dataset::hdf5::Hdf5Reader;
use crate::error::{Error, Result};
use crate::metrics::{LatencyCollector, SearchResult, compute_recall};
use crate::progress::{ProgressEvent, ProgressTx};

pub async fn run(
    config: &BenchmarkConfig,
    registry: &BackendRegistry,
    progress: &ProgressTx,
    filter_index: Option<&str>,
) -> Result<Vec<SearchResult>> {
    let dataset_path = config.dataset.resolved_path();
    let reader = Hdf5Reader::open(
        &dataset_path,
        config.dataset.metric,
        config.dataset.dimension,
    )?;

    let queries = reader.queries()?;
    let ground_truth = reader.ground_truth()?;

    let num_queries = config
        .search
        .num_queries
        .unwrap_or(queries.nrows())
        .min(queries.nrows());

    let k = config.search.k;
    let mut all_results = Vec::new();

    for sweep in &config.search.sweep {
        if let Some(name) = filter_index
            && sweep.index != name
        {
            continue;
        }

        let idx_config = config
            .index
            .iter()
            .find(|i| i.name == sweep.index)
            .ok_or_else(|| Error::SweepIndexNotFound {
                name: sweep.index.clone(),
            })?;

        let backend = registry
            .create(
                &config.connection,
                &config.table_name(),
                config.dataset.dimension,
                config.dataset.metric,
                idx_config,
            )
            .await?;

        if !backend.index_exists().await? {
            return Err(Error::IndexNotFound {
                name: sweep.index.clone(),
            });
        }

        backend.validate_search_params(&sweep.params).await?;

        let param_combos = expand_sweep_params(&sweep.params);

        for params in &param_combos {
            let params_json = serde_json::to_value(params).unwrap_or_default();

            let _ = progress.send(ProgressEvent::SearchStart {
                index_name: sweep.index.clone(),
                params: params_json.clone(),
            });

            backend.set_search_params(params).await?;

            // Warmup
            for qi in 0..config.search.warmup.min(num_queries) {
                let q = queries.row(qi).to_vec();
                let _ = backend.query(&q, k).await?;
            }

            let mut best_collector = LatencyCollector::new();
            let mut best_avg = f64::MAX;
            let mut total_recall = 0.0;

            for run in 0..config.search.runs {
                let mut collector = LatencyCollector::new();
                let mut run_recall = 0.0;

                for qi in 0..num_queries {
                    let q = queries.row(qi).to_vec();
                    let gt = ground_truth.row(qi).to_vec();

                    let start = Instant::now();
                    let result_ids = backend.query(&q, k).await?;
                    let latency = start.elapsed();

                    collector.record(latency.as_micros() as u64);

                    if run == 0 {
                        run_recall += compute_recall(&result_ids, &gt, k);
                    }
                }

                if run == 0 {
                    total_recall = run_recall / num_queries as f64;
                }

                let avg = collector.mean_us();
                if avg < best_avg {
                    best_avg = avg;
                    best_collector = collector;
                }
            }

            let qps = if best_avg > 0.0 {
                1_000_000.0 / best_avg
            } else {
                0.0
            };

            let result = SearchResult {
                index_name: sweep.index.clone(),
                index_type: idx_config.index_type.clone(),
                search_params: params_json.clone(),
                k,
                num_queries,
                num_runs: config.search.runs,
                avg_recall: total_recall,
                qps,
                avg_latency_us: best_collector.mean_us(),
                p50_latency_us: best_collector.percentile_us(50.0),
                p95_latency_us: best_collector.percentile_us(95.0),
                p99_latency_us: best_collector.percentile_us(99.0),
                p999_latency_us: best_collector.percentile_us(99.9),
            };

            let _ = progress.send(ProgressEvent::SearchEnd {
                index_name: sweep.index.clone(),
                recall: result.avg_recall,
                qps: result.qps,
            });

            info!(
                "  {} [{}]: recall={:.4} QPS={:.0}",
                sweep.index, params_json, result.avg_recall, result.qps
            );

            all_results.push(result);
        }
    }

    Ok(all_results)
}

fn expand_sweep_params(
    params: &HashMap<String, Vec<toml::Value>>,
) -> Vec<HashMap<String, toml::Value>> {
    let keys: Vec<&String> = params.keys().collect();
    let values: Vec<&Vec<toml::Value>> = keys.iter().map(|k| &params[*k]).collect();

    if keys.is_empty() {
        return vec![HashMap::new()];
    }

    let mut combos = Vec::new();
    let mut indices = vec![0usize; keys.len()];

    loop {
        let mut combo = HashMap::new();
        for (i, key) in keys.iter().enumerate() {
            combo.insert((*key).clone(), values[i][indices[i]].clone());
        }
        combos.push(combo);

        // Increment indices (Cartesian product)
        let mut carry = true;
        for i in (0..keys.len()).rev() {
            if carry {
                indices[i] += 1;
                if indices[i] >= values[i].len() {
                    indices[i] = 0;
                } else {
                    carry = false;
                }
            }
        }
        if carry {
            break;
        }
    }

    combos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_empty_params() {
        let params = HashMap::new();
        let combos = expand_sweep_params(&params);
        assert_eq!(combos.len(), 1);
        assert!(combos[0].is_empty());
    }

    #[test]
    fn expand_single_param() {
        let mut params = HashMap::new();
        params.insert(
            "mkt.nprobe".to_string(),
            vec![
                toml::Value::Integer(5),
                toml::Value::Integer(10),
                toml::Value::Integer(20),
            ],
        );
        let combos = expand_sweep_params(&params);
        assert_eq!(combos.len(), 3);
        let values: Vec<i64> = combos
            .iter()
            .map(|c| c["mkt.nprobe"].as_integer().unwrap())
            .collect();
        assert!(values.contains(&5));
        assert!(values.contains(&10));
        assert!(values.contains(&20));
    }

    #[test]
    fn expand_cartesian_product() {
        let mut params = HashMap::new();
        params.insert(
            "a".to_string(),
            vec![toml::Value::Integer(1), toml::Value::Integer(2)],
        );
        params.insert(
            "b".to_string(),
            vec![
                toml::Value::Integer(10),
                toml::Value::Integer(20),
                toml::Value::Integer(30),
            ],
        );
        let combos = expand_sweep_params(&params);
        assert_eq!(combos.len(), 6); // 2 * 3
    }

    #[test]
    fn expand_single_value() {
        let mut params = HashMap::new();
        params.insert("x".to_string(), vec![toml::Value::Integer(42)]);
        let combos = expand_sweep_params(&params);
        assert_eq!(combos.len(), 1);
        assert_eq!(combos[0]["x"].as_integer().unwrap(), 42);
    }
}
