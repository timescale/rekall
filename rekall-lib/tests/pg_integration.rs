//! Integration tests requiring a PostgreSQL connection.
//!
//! These tests are skipped unless REKALL_TEST_DSN is set. To run:
//!
//!   REKALL_TEST_DSN="host=/home/user/pg/run/unix-sock port=5434 dbname=postgres" \
//!     cargo test --test pg_integration
//!
//! The target database must have the pgvector extension available.
//! pg_vectorsearch tests also require the pg_vectorsearch extension.

use std::collections::HashMap;

use rekall_lib::backend::postgres::PgConnection;
use rekall_lib::backend::{BackendRegistry, DataLoader};
use rekall_lib::config::*;

fn test_dsn() -> Option<String> {
    std::env::var("REKALL_TEST_DSN").ok()
}

macro_rules! require_pg {
    () => {
        match test_dsn() {
            Some(dsn) => dsn,
            None => {
                eprintln!("Skipping PG test: REKALL_TEST_DSN not set");
                return;
            }
        }
    };
}

fn test_connection(dsn: &str) -> ConnectionConfig {
    ConnectionConfig {
        dsn: dsn.to_string(),
    }
}

// ---------- PgConnection tests ----------

#[tokio::test]
async fn pg_connect_and_create_table() {
    let dsn = require_pg!();
    let pg = PgConnection::new(
        &test_connection(&dsn),
        "_rekall_test_connect",
        3,
        Metric::Angular,
    );

    pg.create_table().await.unwrap();
    assert!(pg.table_exists().await.unwrap());
    assert_eq!(pg.row_count().await.unwrap(), 0);

    // Cleanup
    let client = pg.open_connection().await.unwrap();
    client
        .execute("DROP TABLE IF EXISTS _rekall_test_connect CASCADE", &[])
        .await
        .unwrap();
}

#[tokio::test]
async fn pg_create_meta_tables() {
    let dsn = require_pg!();
    let pg = PgConnection::new(
        &test_connection(&dsn),
        "_rekall_test_meta",
        3,
        Metric::Angular,
    );

    pg.create_meta_tables().await.unwrap();
    // Calling again should succeed (IF NOT EXISTS)
    pg.create_meta_tables().await.unwrap();

    let client = pg.open_connection().await.unwrap();
    client
        .execute("DROP TABLE IF EXISTS _rekall_meta CASCADE", &[])
        .await
        .unwrap();
    client
        .execute("DROP TABLE IF EXISTS _rekall_load_progress CASCADE", &[])
        .await
        .unwrap();
}

#[tokio::test]
async fn pg_guc_validation_bad_name() {
    let dsn = require_pg!();
    let pg = PgConnection::new(
        &test_connection(&dsn),
        "_rekall_test_guc",
        3,
        Metric::Angular,
    );

    // Use open_connection to get a client, then test GUC validation
    // directly without needing a prepared query statement.
    let client = pg.open_connection().await.unwrap();

    // A non-dotted name that doesn't exist should fail on SET
    let result = client.execute("SET nonexistent_guc = 1", &[]).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn pg_guc_set_builtin() {
    let dsn = require_pg!();
    let pg = PgConnection::new(
        &test_connection(&dsn),
        "_rekall_test_guc2",
        3,
        Metric::Angular,
    );

    let client = pg.open_connection().await.unwrap();
    client.execute("SET work_mem = '64MB'", &[]).await.unwrap();
}

#[tokio::test]
async fn pg_guc_pg_vectorsearch_after_load() {
    let dsn = require_pg!();
    if !has_pg_vectorsearch() {
        eprintln!("Skipping pg_vectorsearch GUC test: REKALL_TEST_PG_VECTORSEARCH=0");
        return;
    }

    let pg = PgConnection::new(
        &test_connection(&dsn),
        "_rekall_test_guc_prism",
        3,
        Metric::Angular,
    );

    let client = pg.open_connection().await.unwrap();
    // Creating the extension loads its versioned library and registers the
    // PRISM GUCs; the library name is deliberately not assumed here.
    client
        .execute("CREATE EXTENSION IF NOT EXISTS pg_vectorsearch", &[])
        .await
        .unwrap();
    // Now prism.nprobe should be settable
    client.execute("SET prism.nprobe = 10", &[]).await.unwrap();
    // Invalid pg_vectorsearch GUC should fail
    let result = client.execute("SET prism.nonexistent = 1", &[]).await;
    assert!(result.is_err());
}

// ---------- IndexBackend tests ----------

fn has_pg_vectorsearch() -> bool {
    std::env::var("REKALL_TEST_PG_VECTORSEARCH")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true)
}

#[tokio::test]
async fn backend_pgvector_hnsw_lifecycle() {
    let dsn = require_pg!();
    let conn = test_connection(&dsn);
    let table = "_rekall_test_hnsw";
    let dim = 3;

    // Setup: create table and insert a few vectors
    let pg = PgConnection::new(&conn, table, dim, Metric::Euclidean);
    pg.create_table().await.unwrap();

    let client = pg.open_connection().await.unwrap();
    for i in 0..100_i64 {
        let v = format!("[{}, {}, {}]", i as f32, (i * 2) as f32, (i * 3) as f32);
        client
            .execute(
                &format!("INSERT INTO {table} (id, v) VALUES ({i}, '{v}'::vector)"),
                &[],
            )
            .await
            .unwrap();
    }

    let idx_config = IndexConfig {
        index_type: "pgvector-hnsw".to_string(),
        name: "test-hnsw".to_string(),
        params: toml::Value::try_from(toml::toml! {
            m = 4
            ef_construction = 16
        })
        .unwrap(),
        gucs: HashMap::new(),
        table_parallel_workers: None,
        physical_index: None,
    };

    let registry = BackendRegistry::new();
    let backend = registry
        .create(
            &conn,
            table,
            dim,
            Metric::Euclidean,
            &idx_config,
            None,
            rekall_lib::config::QueryMode::Default,
            None,
        )
        .await
        .unwrap();

    // Build
    assert!(!backend.index_exists().await.unwrap());
    let build_result = backend.build_index().await.unwrap();
    assert_eq!(build_result.index_name, "test-hnsw");
    assert!(build_result.index_size_bytes > 0);
    assert!(backend.index_exists().await.unwrap());

    // Set search params
    let mut search_params = HashMap::new();
    search_params.insert("hnsw.ef_search".to_string(), toml::Value::Integer(10));
    backend.set_search_params(&search_params).await.unwrap();

    // Query
    let results = backend.query(&[0.0, 0.0, 0.0], 5).await.unwrap();
    assert_eq!(results.len(), 5);

    // Drop
    backend.drop_index().await.unwrap();
    assert!(!backend.index_exists().await.unwrap());

    // Cleanup
    client
        .execute(&format!("DROP TABLE IF EXISTS {table} CASCADE"), &[])
        .await
        .unwrap();
}

#[tokio::test]
async fn backend_pg_vectorsearch_lifecycle() {
    let dsn = require_pg!();
    if !has_pg_vectorsearch() {
        eprintln!("Skipping pg_vectorsearch test: REKALL_TEST_PG_VECTORSEARCH=0");
        return;
    }

    let conn = test_connection(&dsn);
    let table = "_rekall_test_prism";
    let dim = 3;

    let pg = PgConnection::new(&conn, table, dim, Metric::Euclidean);
    pg.create_table().await.unwrap();

    let client = pg.open_connection().await.unwrap();
    for i in 0..200_i64 {
        let v = format!("[{}, {}, {}]", i as f32, (i * 2) as f32, (i * 3) as f32);
        client
            .execute(
                &format!("INSERT INTO {table} (id, v) VALUES ({i}, '{v}'::vector)"),
                &[],
            )
            .await
            .unwrap();
    }

    let idx_config = IndexConfig {
        index_type: "prism".to_string(),
        name: "test-prism".to_string(),
        params: toml::Value::try_from(toml::toml! {
            nlist = 4
        })
        .unwrap(),
        gucs: HashMap::new(),
        table_parallel_workers: None,
        physical_index: None,
    };

    let registry = BackendRegistry::new();
    let backend = registry
        .create(
            &conn,
            table,
            dim,
            Metric::Euclidean,
            &idx_config,
            None,
            rekall_lib::config::QueryMode::Default,
            None,
        )
        .await
        .unwrap();

    assert!(!backend.index_exists().await.unwrap());
    let build_result = backend.build_index().await.unwrap();
    assert_eq!(build_result.index_name, "test-prism");
    assert!(build_result.index_size_bytes > 0);

    let mut search_params = HashMap::new();
    search_params.insert("prism.nprobe".to_string(), toml::Value::Integer(2));
    backend.set_search_params(&search_params).await.unwrap();

    let results = backend.query(&[0.0, 0.0, 0.0], 5).await.unwrap();
    assert_eq!(results.len(), 5);

    backend.drop_index().await.unwrap();
    assert!(!backend.index_exists().await.unwrap());

    client
        .execute(&format!("DROP TABLE IF EXISTS {table} CASCADE"), &[])
        .await
        .unwrap();
}

// ---------- Search phase tests ----------

/// Write a small Euclidean dataset: `train` vectors, `test` queries and
/// their exact top-10 `neighbors` and `distances` by brute force. The file
/// is closed before the path is handed on, since HDF5 holds an exclusive
/// lock while it is open for writing.
fn write_search_dataset(path: &std::path::Path, rows: usize, queries: usize, dim: usize) {
    use hdf5::plist::file_access::FileCloseDegree;
    use ndarray::Array2;

    // Deterministic pseudo-random values in [0, 1).
    let mut state = 0x2545_f491_u64;
    let mut next = move || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 40) as f32 / (1u64 << 24) as f32
    };
    let train = Array2::from_shape_fn((rows, dim), |_| next());
    let test = Array2::from_shape_fn((queries, dim), |_| next());

    let k = 10;
    let mut neighbors = Array2::<i32>::zeros((queries, k));
    let mut distances = Array2::<f32>::zeros((queries, k));
    for q in 0..queries {
        let mut by_distance: Vec<(f32, usize)> = (0..rows)
            .map(|r| {
                let d2: f32 = (0..dim)
                    .map(|c| (train[[r, c]] - test[[q, c]]).powi(2))
                    .sum();
                (d2.sqrt(), r)
            })
            .collect();
        by_distance.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (j, (d, r)) in by_distance.into_iter().take(k).enumerate() {
            neighbors[[q, j]] = r as i32;
            distances[[q, j]] = d;
        }
    }

    let file = hdf5::File::with_options()
        .with_fapl(|fapl| fapl.fclose_degree(FileCloseDegree::Strong))
        .create(path)
        .unwrap();
    for (name, data) in [
        ("train", &train),
        ("test", &test),
        ("distances", &distances),
    ] {
        file.new_dataset::<f32>()
            .shape(data.dim())
            .create(name)
            .unwrap()
            .write(data)
            .unwrap();
    }
    file.new_dataset::<i32>()
        .shape(neighbors.dim())
        .create("neighbors")
        .unwrap()
        .write(&neighbors)
        .unwrap();
    file.close().unwrap();
}

/// Open the fixture read-only the way rekall's reader does, without HDF5
/// file locking: HDF5 refuses a second open of a file in one process with
/// different locking flags.
fn open_dataset(path: &std::path::Path) -> hdf5::File {
    hdf5::File::with_options()
        .with_fapl(|fapl| fapl.file_locking(false))
        .open(path)
        .unwrap()
}

async fn client_backends(dsn: &str) -> i64 {
    let (client, connection) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(connection);
    client
        .query_one(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE backend_type = 'client backend' AND pid <> pg_backend_pid()",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_phase_measures_concurrency_at_operating_points() {
    let dsn = require_pg!();
    let dir = tempfile::tempdir().unwrap();
    let dataset = dir.path().join("data.hdf5");
    // Enough rows that the planner prefers the index over a seq scan.
    let (rows, num_queries, dim) = (10_000, 20, 8);
    write_search_dataset(&dataset, rows, num_queries, dim);

    let config: BenchmarkConfig = toml::from_str(&format!(
        r#"
[dataset]
name = "rekall-test-concurrency"
path = "{dataset}"
metric = "euclidean"
dimension = {dim}

[connection]
dsn = "{dsn}"

[[index]]
type = "pgvector-hnsw"
name = "hnsw"
[index.params]
m = 8
ef_construction = 32

# Reuses the "hnsw" index under its own name.
[[index]]
type = "pgvector-hnsw"
name = "hnsw-alias"
physical_index = "hnsw"
[index.params]
m = 8
ef_construction = 32

[[index]]
type = "pgvector-hnsw"
name = "hnsw-half"
[index.params]
m = 8
ef_construction = 32
encoding = "halfvec"

[search]
k = 10
runs = 2
warmup = 5
clients = [4]
# ef_search 5 returns at most 5 of the 10 neighbours, so its recall stays
# at or below 0.5: it can serve the low target but never the high one,
# which puts "hnsw" on two operating points.
concurrency_recall_targets = [0.3, 1.0]

[[search.sweep]]
index = "hnsw"
[search.sweep.params]
"hnsw.ef_search" = [5, 40, 200]

# Below pgvector's default ef_search of 40, so a pool client left at the
# default would return different results.
[[query]]
name = "hnsw-alias"
index = "hnsw-alias"
[query.params]
"hnsw.ef_search" = [10]

[[query]]
name = "half-rerank"
index = "hnsw-half"
mode = "rerank"
[query.params]
"hnsw.ef_search" = [10]
rerank_pool = [20]

[output]
results_dir = "{results}"
"#,
        dataset = dataset.display(),
        results = dir.path().join("results").display(),
    ))
    .unwrap();
    let table = config.table_name();

    // Load the rows with ids matching the ground truth's row numbers.
    let pg = PgConnection::new(&config.connection, &table, dim as u32, Metric::Euclidean);
    pg.create_table().await.unwrap();
    let client = pg.open_connection().await.unwrap();
    let train: ndarray::Array2<f32> = open_dataset(&dataset)
        .dataset("train")
        .unwrap()
        .read_2d()
        .unwrap();
    let values: Vec<String> = train
        .outer_iter()
        .enumerate()
        .map(|(i, row)| {
            let v: Vec<String> = row.iter().map(|x| x.to_string()).collect();
            format!("({i}, '[{}]'::vector)", v.join(","))
        })
        .collect();
    client
        .execute(
            &format!("INSERT INTO {table} (id, v) VALUES {}", values.join(",")),
            &[],
        )
        .await
        .unwrap();
    client
        .execute(&format!("ANALYZE {table}"), &[])
        .await
        .unwrap();

    // Build the two physical indexes with the DDL the backends describe.
    // `build_index` is avoided: its build monitor never sees a build this
    // small finish and waits forever.
    let registry = BackendRegistry::new();
    for name in ["hnsw", "hnsw-half"] {
        let idx = config.index.iter().find(|i| i.name == name).unwrap();
        let backend = registry
            .create(
                &config.connection,
                &table,
                dim as u32,
                Metric::Euclidean,
                idx,
                None,
                QueryMode::Default,
                None,
            )
            .await
            .unwrap();
        let options: Vec<String> = backend
            .index_options()
            .iter()
            .map(|(k, v)| format!("{k} = {v}"))
            .collect();
        client
            .execute(
                &format!(
                    "CREATE INDEX {} ON {table} USING {} ({} {}) WITH ({})",
                    backend.pg().index_sql_name(name),
                    backend.index_method(),
                    backend.index_target(),
                    backend.opclass(),
                    options.join(", ")
                ),
                &[],
            )
            .await
            .unwrap();
    }

    let connections_before = client_backends(&dsn).await;
    let run_search = |config: BenchmarkConfig| {
        let registry = &registry;
        async move {
            let (tx, _rx) = rekall_lib::progress::progress_channel();
            rekall_lib::phase::search::run(&config, registry, &tx, None, None, None, &[], false)
                .await
                .unwrap();
            rekall_lib::phase::search::load_existing_results(&config)
        }
    };
    let first = run_search(config.clone()).await;

    let (gt_neighbors, gt_distances) = {
        let file = open_dataset(&dataset);
        let neighbors: ndarray::Array2<i32> = file.dataset("neighbors").unwrap().read_2d().unwrap();
        let distances: ndarray::Array2<f32> = file.dataset("distances").unwrap().read_2d().unwrap();
        (neighbors, distances)
    };
    let gt_rows: Vec<(Vec<i64>, Vec<f32>)> = (0..num_queries)
        .map(|qi| {
            (
                gt_neighbors.row(qi).iter().map(|&n| n as i64).collect(),
                gt_distances.row(qi).to_vec(),
            )
        })
        .collect();
    let distance_recall = |sweep: &rekall_lib::metrics::SearchResult| {
        rekall_lib::metrics::mean_recall_distance_based(&sweep.query_results, 10, |qi| {
            let (n, d) = gt_rows.get(qi)?;
            Some((n.as_slice(), d.as_slice()))
        })
        .unwrap()
    };
    // Each target's operating point, worked out from the sweep: the fastest
    // point whose recall reaches it, with the targets it serves.
    let operating_points = |series: &str, targets: &[f64]| {
        let mut points: Vec<(serde_json::Value, Vec<f64>)> = Vec::new();
        for &target in targets {
            let best = first
                .search
                .iter()
                .filter(|s| s.index_name == series && distance_recall(s) >= target)
                .max_by(|a, b| a.qps.total_cmp(&b.qps));
            if let Some(best) = best {
                match points.iter_mut().find(|p| p.0 == best.search_params) {
                    Some(p) => p.1.push(target),
                    None => points.push((best.search_params.clone(), vec![target])),
                }
            }
        }
        points.sort_by_key(|p| p.0.to_string());
        points
    };
    let measured_points = |results: &rekall_lib::phase::search::SearchResults, series: &str| {
        let mut points: Vec<(serde_json::Value, Vec<f64>)> = Vec::new();
        for r in results
            .concurrency
            .iter()
            .filter(|r| r.index_name == series)
        {
            if !points.iter().any(|p| p.0 == r.search_params) {
                points.push((r.search_params.clone(), r.recall_targets.clone()));
            }
        }
        points.sort_by_key(|p| p.0.to_string());
        points
    };
    let clients_at = |results: &rekall_lib::phase::search::SearchResults,
                      series: &str,
                      params: &serde_json::Value| {
        let mut clients: Vec<usize> = results
            .concurrency
            .iter()
            .filter(|r| r.index_name == series && r.search_params == *params)
            .map(|r| r.clients)
            .collect();
        clients.sort();
        clients
    };

    assert_eq!(
        operating_points("hnsw", &[0.3, 1.0]).len(),
        2,
        "the fixture must put hnsw's targets on two points"
    );
    for series in ["hnsw", "hnsw-alias", "half-rerank"] {
        let points = operating_points(series, &[0.3, 1.0]);
        assert!(!points.is_empty(), "{series} reaches no target");
        assert_eq!(measured_points(&first, series), points, "{series}");
        for (params, _) in &points {
            assert_eq!(clients_at(&first, series, params), vec![1, 4], "{series}");
        }
        for r in first.concurrency.iter().filter(|r| r.index_name == series) {
            // Read-only queries return what the single-client sweep did, so
            // recall matches the sweep's, counted the published way.
            let sweep = first
                .search
                .iter()
                .find(|s| s.index_name == series && s.search_params == r.search_params)
                .unwrap();
            assert_eq!(r.recall_basis, rekall_lib::metrics::RecallBasis::Distance);
            assert!(
                (r.avg_recall - distance_recall(sweep)).abs() < 1e-12,
                "{series}: concurrent recall {} vs sweep {}",
                r.avg_recall,
                distance_recall(sweep)
            );
            // At least 10 queries per client, cycling the 20-query set.
            let expected = num_queries.max(10 * r.clients) as u64;
            assert_eq!(r.queries_finished_total, expected, "{series}");
            assert!(r.queries_finished_till_first_empty <= expected);
            assert!(r.qps_wall > 0.0 && r.throughput_qps > 0.0);
            assert_eq!(r.cpu_count, rekall_lib::environment::collect_cpu_count());
            assert_eq!(r.num_runs, 2);
            assert_eq!(r.cache.as_deref(), Some("warm"));
        }
    }

    // Every pool connection is closed once the phase returns.
    let mut connections_after = client_backends(&dsn).await;
    for _ in 0..50 {
        if connections_after <= connections_before {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        connections_after = client_backends(&dsn).await;
    }
    assert!(
        connections_after <= connections_before,
        "{connections_after} client connections left open, {connections_before} before"
    );

    // Resume with only the lower target and an extra client level: the point
    // that served 1.0 alone is dropped, the others keep their 1- and
    // 4-client results relabelled, and only 6 clients are measured.
    let mut narrowed = config.clone();
    narrowed.search.concurrency_recall_targets = vec![0.3];
    narrowed.search.clients = vec![ClientCount::Fixed(4), ClientCount::Fixed(6)];
    // Result files are named by the second they were written.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let second = run_search(narrowed.clone()).await;
    for series in ["hnsw", "hnsw-alias", "half-rerank"] {
        let points = operating_points(series, &[0.3]);
        assert_eq!(measured_points(&second, series), points, "{series}");
        let (params, _) = &points[0];
        assert_eq!(clients_at(&second, series, params), vec![1, 4, 6]);
        for clients in [1, 4] {
            let before = first
                .concurrency
                .iter()
                .find(|r| {
                    r.index_name == series && r.search_params == *params && r.clients == clients
                })
                .unwrap();
            let after = second
                .concurrency
                .iter()
                .find(|r| {
                    r.index_name == series && r.search_params == *params && r.clients == clients
                })
                .unwrap();
            assert_eq!(
                after.qps_wall, before.qps_wall,
                "{series} {clients} re-measured"
            );
        }
    }

    // A third run finds everything measured: nothing new is written.
    let files = || {
        let mut names: Vec<String> = std::fs::read_dir(dir.path().join("results"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let files_before = files();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let third = run_search(narrowed).await;
    assert_eq!(files(), files_before);
    assert_eq!(third.concurrency.len(), second.concurrency.len());
    assert_eq!(third.search.len(), second.search.len());

    client
        .execute(&format!("DROP TABLE IF EXISTS {table} CASCADE"), &[])
        .await
        .unwrap();
}
