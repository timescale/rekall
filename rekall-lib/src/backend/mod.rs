pub mod meerkat;
pub mod pgvector_hnsw;
pub mod postgres;

use std::collections::HashMap;

use async_trait::async_trait;

use crate::config::{ConnectionConfig, IndexConfig, Metric};
use crate::error::{Error, Result};
use crate::metrics::IndexBuildResult;
use crate::progress::ProgressTx;

#[async_trait]
pub trait IndexBackend: Send + Sync {
    fn type_name(&self) -> &str;

    fn index_name(&self) -> &str;

    async fn build_index(&self) -> Result<IndexBuildResult>;

    async fn drop_index(&self) -> Result<()>;

    async fn index_size(&self) -> Result<u64>;

    async fn index_exists(&self) -> Result<bool>;

    async fn set_search_params(
        &self,
        params: &HashMap<String, toml::Value>,
    ) -> Result<()>;

    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>>;
}

pub type BackendFactory = Box<
    dyn Fn(
            &ConnectionConfig,
            &str,       // table_name
            u32,        // dimension
            Metric,
            &IndexConfig,
        ) -> Result<Box<dyn IndexBackend>>
        + Send
        + Sync,
>;

pub struct BackendRegistry {
    factories: HashMap<String, BackendFactory>,
}

impl BackendRegistry {
    pub fn new() -> Self {
        let mut reg = Self {
            factories: HashMap::new(),
        };
        reg.register(
            "meerkat",
            Box::new(|conn, table, dim, metric, idx| {
                Ok(Box::new(meerkat::MeerkatBackend::new(
                    conn, table, dim, metric, idx,
                )?))
            }),
        );
        reg.register(
            "pgvector-hnsw",
            Box::new(|conn, table, dim, metric, idx| {
                Ok(Box::new(pgvector_hnsw::PgvectorHnswBackend::new(
                    conn, table, dim, metric, idx,
                )?))
            }),
        );
        reg
    }

    pub fn register(&mut self, type_name: &str, factory: BackendFactory) {
        self.factories.insert(type_name.to_string(), factory);
    }

    pub fn create(
        &self,
        conn: &ConnectionConfig,
        table: &str,
        dimension: u32,
        metric: Metric,
        index_config: &IndexConfig,
    ) -> Result<Box<dyn IndexBackend>> {
        let factory = self.factories.get(&index_config.index_type).ok_or_else(|| {
            Error::UnknownBackend {
                backend_type: index_config.index_type.clone(),
            }
        })?;
        factory(conn, table, dimension, metric, index_config)
    }
}

/// Shared interface for data loading (PG backends share this via PgConnection).
#[async_trait]
pub trait DataLoader: Send + Sync {
    async fn table_exists(&self) -> Result<bool>;
    async fn row_count(&self) -> Result<u64>;
    async fn create_table(&self) -> Result<()>;
    async fn create_meta_tables(&self) -> Result<()>;

    /// Load vectors from a channel of chunks. The caller is responsible for
    /// streaming chunks from the dataset reader (in a blocking thread) into
    /// the provided channel receiver.
    async fn load_data(
        &self,
        total: u64,
        chunk_rx: &mut tokio::sync::mpsc::Receiver<Result<crate::dataset::VectorChunk>>,
        progress: &ProgressTx,
    ) -> Result<u64>;
}
