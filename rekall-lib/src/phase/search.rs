use std::collections::HashMap;
use std::time::Instant;

use tracing::info;

use crate::backend::BackendRegistry;
use crate::config::{BenchmarkConfig, CacheMode};
use crate::dataset::hdf5::Hdf5Reader;
use crate::dataset::DatasetReader;
use crate::error::{Error, Result};
use crate::metrics::{compute_recall, LatencyCollector, SearchResult};
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
        if let Some(name) = filter_index {
            if sweep.index != name {
                continue;
            }
        }

        let idx_config = config
            .index
            .iter()
            .find(|i| i.name == sweep.index)
            .ok_or_else(|| Error::SweepIndexNotFound {
                name: sweep.index.clone(),
            })?;

        // Disable all other rekall indexes, enable only the active one
        let mut pg_mgmt = crate::backend::postgres::PgConnection::new(
            &config.connection,
            &config.table_name(),
            config.dataset.dimension,
            config.dataset.metric,
        );
        pg_mgmt.connect("<=>", None).await?;
        let active_idx = pg_mgmt.index_sql_name(&sweep.index);
        let all_indexes = pg_mgmt.list_rekall_indexes().await?;
        for idx_sql in &all_indexes {
            if *idx_sql == active_idx {
                pg_mgmt.enable_index(idx_sql).await?;
            } else {
                pg_mgmt.disable_index(idx_sql).await?;
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
            )
            .await?;

        if !backend.index_exists().await? {
            return Err(Error::IndexNotFound {
                name: sweep.index.clone(),
            });
        }

        // Validate default GUCs from index config
        let default_gucs_for_validation: HashMap<String, Vec<toml::Value>> = idx_config
            .gucs
            .iter()
            .map(|(k, v)| (k.clone(), vec![v.clone()]))
            .collect();
        if !default_gucs_for_validation.is_empty() {
            backend
                .validate_search_params(&default_gucs_for_validation)
                .await?;
        }
        backend.validate_search_params(&sweep.params).await?;

        // Validate query plan uses the expected index
        backend
            .validate_query_plan(&queries.row(0).to_vec(), k)
            .await?;
        let _ = progress.send(ProgressEvent::Message {
            text: format!("Query plan validated for '{}'", sweep.index),
        });

        let param_combos = expand_sweep_params(&sweep.params);

        for params in &param_combos {
            let params_json = serde_json::to_value(params).unwrap_or_default();

            let _ = progress.send(ProgressEvent::SearchStart {
                index_name: sweep.index.clone(),
                params: params_json.clone(),
            });

            // Apply default GUCs from index config first, then sweep overrides
            backend.set_search_params(&idx_config.gucs).await?;
            backend.set_search_params(params).await?;

            match config.search.cache {
                CacheMode::Warm => {
                    // Warmup queries to fill shared buffers
                    for qi in 0..config.search.warmup.min(num_queries) {
                        let q = queries.row(qi).to_vec();
                        let _ = backend.query(&q, k).await?;
                    }
                }
                CacheMode::Cold => {
                    // Evict shared buffers before measurement.
                    // Skip warmup to measure cold-cache performance.
                    let mut pg_evict = crate::backend::postgres::PgConnection::new(
                        &config.connection,
                        &config.table_name(),
                        config.dataset.dimension,
                        config.dataset.metric,
                    );
                    pg_evict.connect("<=>", None).await?;
                    let idx_sql = pg_evict.index_sql_name(&sweep.index);
                    pg_evict
                        .evict_relation_buffers(&config.table_name())
                        .await?;
                    pg_evict.evict_relation_buffers(&idx_sql).await?;

                    let _ = progress.send(ProgressEvent::Message {
                        text: "Evicted shared buffers (cold mode)".to_string(),
                    });
                }
            }

            let mut best_collector = LatencyCollector::new();
            let mut best_avg = f64::MAX;
            let mut best_wall_secs = 0.0;
            let mut total_recall = 0.0;

            for run in 0..config.search.runs {
                let mut collector = LatencyCollector::new();
                let mut run_recall = 0.0;

                let wall_start = Instant::now();
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
                let wall_secs = wall_start.elapsed().as_secs_f64();

                if run == 0 {
                    total_recall = run_recall / num_queries as f64;
                }

                let avg = collector.mean_us();
                if avg < best_avg {
                    best_avg = avg;
                    best_collector = collector;
                    best_wall_secs = wall_secs;
                }
            }

            let qps = if best_avg > 0.0 {
                1_000_000.0 / best_avg
            } else {
                0.0
            };
            let qps_wall = if best_wall_secs > 0.0 {
                num_queries as f64 / best_wall_secs
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
                wall_time_secs: best_wall_secs,
                qps_wall,
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

    // Re-enable all indexes when done
    let mut pg_cleanup = crate::backend::postgres::PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );
    pg_cleanup.connect("<=>", None).await?;
    for idx_sql in pg_cleanup.list_rekall_indexes().await? {
        let _ = pg_cleanup.enable_index(&idx_sql).await;
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
