pub mod meerkat;
pub mod pg_vector_type;
pub mod pgvector_hnsw;
pub mod postgres;
pub mod vchordrq;

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

    /// Set search-time parameters. For PG backends, this sets GUCs
    /// via PgConnection::set_gucs(). Remote backends override this
    /// with their own parameter-setting mechanism.
    async fn set_search_params(&self, params: &HashMap<String, toml::Value>) -> Result<()>;

    /// Validate that search param names are valid before starting a sweep.
    async fn validate_search_params(
        &self,
        _params: &HashMap<String, Vec<toml::Value>>,
    ) -> Result<()> {
        Ok(())
    }

    async fn query(&self, vector: &[f32], k: usize) -> Result<Vec<i64>>;

    /// Validate that the query plan uses the expected index.
    /// PG backends should override this. Default is a no-op for remote backends.
    async fn validate_query_plan(&self, _sample_vector: &[f32], _k: usize) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct BackendRegistry;

impl BackendRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub async fn create(
        &self,
        conn: &ConnectionConfig,
        table: &str,
        dimension: u32,
        metric: Metric,
        index_config: &IndexConfig,
        cancel_registry: Option<&crate::CancelRegistry>,
    ) -> Result<Box<dyn IndexBackend>> {
        match index_config.index_type.as_str() {
            "meerkat" => {
                let backend = meerkat::MeerkatBackend::create(
                    conn,
                    table,
                    dimension,
                    metric,
                    index_config,
                    cancel_registry,
                )
                .await?;
                Ok(Box::new(backend) as Box<dyn IndexBackend>)
            }
            "pgvector-hnsw" => {
                let backend = pgvector_hnsw::PgvectorHnswBackend::create(
                    conn,
                    table,
                    dimension,
                    metric,
                    index_config,
                    cancel_registry,
                )
                .await?;
                Ok(Box::new(backend) as Box<dyn IndexBackend>)
            }
            t @ ("vchordrq" | "vchordrq-rabitq8" | "vchordrq-rabitq4") => {
                let variant = match t {
                    "vchordrq" => vchordrq::Variant::Plain,
                    "vchordrq-rabitq8" => vchordrq::Variant::Rabitq8,
                    "vchordrq-rabitq4" => vchordrq::Variant::Rabitq4Rerank,
                    _ => unreachable!(),
                };
                let backend = vchordrq::VchordrqBackend::create(
                    conn,
                    table,
                    dimension,
                    metric,
                    index_config,
                    cancel_registry,
                    variant,
                )
                .await?;
                Ok(Box::new(backend) as Box<dyn IndexBackend>)
            }
            other => Err(Error::UnknownBackend {
                backend_type: other.to_string(),
            }),
        }
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
