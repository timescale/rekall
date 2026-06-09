//! Integration tests requiring a PostgreSQL connection.
//!
//! These tests are skipped unless REKALL_TEST_DSN is set. To run:
//!
//!   REKALL_TEST_DSN="host=/home/user/pg/run/unix-sock port=5434 dbname=postgres" \
//!     cargo test --test pg_integration
//!
//! The target database must have the pgvector extension available.
//! Meerkat tests also require the meerkat extension.

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
async fn pg_guc_meerkat_after_load() {
    let dsn = require_pg!();
    if !has_meerkat() {
        eprintln!("Skipping meerkat GUC test: REKALL_TEST_MEERKAT=0");
        return;
    }

    let pg = PgConnection::new(
        &test_connection(&dsn),
        "_rekall_test_guc_mkt",
        3,
        Metric::Angular,
    );

    let client = pg.open_connection().await.unwrap();
    // LOAD registers meerkat GUCs
    client.execute("LOAD 'meerkat'", &[]).await.unwrap();
    // Now mkt.nprobe should be settable
    client.execute("SET mkt.nprobe = 10", &[]).await.unwrap();
    // Invalid meerkat GUC should fail
    let result = client.execute("SET mkt.nonexistent = 1", &[]).await;
    assert!(result.is_err());
}

// ---------- IndexBackend tests ----------

fn has_meerkat() -> bool {
    std::env::var("REKALL_TEST_MEERKAT")
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
                &format!("INSERT INTO {table} (v) VALUES ('{v}'::vector)"),
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
async fn backend_meerkat_lifecycle() {
    let dsn = require_pg!();
    if !has_meerkat() {
        eprintln!("Skipping meerkat test: REKALL_TEST_MEERKAT=0");
        return;
    }

    let conn = test_connection(&dsn);
    let table = "_rekall_test_mkt";
    let dim = 3;

    let pg = PgConnection::new(&conn, table, dim, Metric::Euclidean);
    pg.create_table().await.unwrap();

    let client = pg.open_connection().await.unwrap();
    for i in 0..200_i64 {
        let v = format!("[{}, {}, {}]", i as f32, (i * 2) as f32, (i * 3) as f32);
        client
            .execute(
                &format!("INSERT INTO {table} (v) VALUES ('{v}'::vector)"),
                &[],
            )
            .await
            .unwrap();
    }

    let idx_config = IndexConfig {
        index_type: "meerkat".to_string(),
        name: "test-mkt".to_string(),
        params: toml::Value::try_from(toml::toml! {
            nlist = 4
        })
        .unwrap(),
        gucs: HashMap::new(),
        table_parallel_workers: None,
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
            None,
        )
        .await
        .unwrap();

    assert!(!backend.index_exists().await.unwrap());
    let build_result = backend.build_index().await.unwrap();
    assert_eq!(build_result.index_name, "test-mkt");
    assert!(build_result.index_size_bytes > 0);

    let mut search_params = HashMap::new();
    search_params.insert("mkt.nprobe".to_string(), toml::Value::Integer(2));
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
