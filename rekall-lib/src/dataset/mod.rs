pub mod download;
pub mod emit;
pub mod hdf5;

use ndarray::Array2;
use serde::{Deserialize, Serialize};

use crate::config::Metric;
use crate::error::Result;

/// Float width the source file stores its vectors in.
///
/// This does not change what rekall reads: HDF5 converts on the way out of
/// `H5Dread`, so every vector reaches us as `f32` whatever the file holds.
/// It exists so the heap column can *match* the source rather than pad it.
/// Expanding f16 source data into a `vector` column stores bit-identical
/// values in twice the space -- same recall, twice the heap a scan competes
/// with for cache, twice the pages a rerank touches -- so an f16 source
/// defaults to a `halfvec` column.
///
/// Narrowing is a different matter and is never implicit: f64 sources (glove)
/// already lose precision reaching `f32`, and going on to f16 is a real
/// tradeoff, so it stays an explicit `storage = "halfvec"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourcePrecision {
    F16,
    F32,
    F64,
}

impl SourcePrecision {
    pub fn bits(self) -> u32 {
        match self {
            SourcePrecision::F16 => 16,
            SourcePrecision::F32 => 32,
            SourcePrecision::F64 => 64,
        }
    }
}

impl std::fmt::Display for SourcePrecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "f{}", self.bits())
    }
}

fn default_source_precision() -> SourcePrecision {
    SourcePrecision::F32
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetMeta {
    pub num_vectors: u64,
    pub dimension: u32,
    pub num_queries: u64,
    pub metric: Metric,
    /// What the file stores, not what we read (always f32). Defaulted for
    /// results written before this was recorded.
    #[serde(default = "default_source_precision")]
    pub source_precision: SourcePrecision,
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

    fn ground_truth_distances(&self) -> Result<Option<Array2<f32>>> {
        Ok(None)
    }
}
