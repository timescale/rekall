pub mod hdf5;

use ndarray::Array2;
use serde::{Deserialize, Serialize};

use crate::config::Metric;
use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetMeta {
    pub num_vectors: u64,
    pub dimension: u32,
    pub num_queries: u64,
    pub metric: Metric,
}

pub struct VectorChunk {
    pub start_index: u64,
    pub vectors: Array2<f32>,
}

pub trait DatasetReader: Send {
    fn meta(&self) -> &DatasetMeta;

    fn train_chunks(&self, chunk_size: usize)
        -> Box<dyn Iterator<Item = Result<VectorChunk>> + '_>;

    fn train_range(
        &self,
        start: u64,
        end: u64,
        chunk_size: usize,
    ) -> Box<dyn Iterator<Item = Result<VectorChunk>> + '_>;

    fn queries(&self) -> Result<Array2<f32>>;

    fn ground_truth(&self) -> Result<Array2<i64>>;
}
