use std::path::{Path, PathBuf};

use ndarray::Array2;

use crate::config::Metric;
use crate::error::{Error, Result};

use super::{DatasetMeta, DatasetReader, VectorChunk};

pub struct Hdf5Reader {
    path: PathBuf,
    meta: DatasetMeta,
}

impl Hdf5Reader {
    pub fn open(path: &Path, metric: Metric, dimension: u32) -> Result<Self> {
        if !path.exists() {
            return Err(Error::DatasetNotFound {
                path: path.to_path_buf(),
            });
        }

        let file = hdf5::File::open(path).map_err(|e| Error::Hdf5 {
            path: path.to_path_buf(),
            detail: format!("cannot open file: {e}"),
        })?;

        let train = file.dataset("train").map_err(|e| Error::Hdf5 {
            path: path.to_path_buf(),
            detail: format!("missing 'train' dataset: {e}"),
        })?;
        let test = file.dataset("test").map_err(|e| Error::Hdf5 {
            path: path.to_path_buf(),
            detail: format!("missing 'test' dataset: {e}"),
        })?;

        let train_shape = train.shape();
        let test_shape = test.shape();

        if train_shape.len() != 2 {
            return Err(Error::Hdf5 {
                path: path.to_path_buf(),
                detail: format!("'train' has {}-d shape, expected 2-d", train_shape.len()),
            });
        }

        let num_vectors = train_shape[0] as u64;
        let file_dim = train_shape[1] as u32;
        let num_queries = test_shape[0] as u64;

        if file_dim != dimension {
            return Err(Error::Hdf5 {
                path: path.to_path_buf(),
                detail: format!(
                    "dimension mismatch: file has {file_dim}, \
                     config says {dimension}"
                ),
            });
        }

        Ok(Self {
            path: path.to_path_buf(),
            meta: DatasetMeta {
                num_vectors,
                dimension,
                num_queries,
                metric,
            },
        })
    }

    pub fn info(path: &Path) -> Result<DatasetInfo> {
        if !path.exists() {
            return Err(Error::DatasetNotFound {
                path: path.to_path_buf(),
            });
        }

        let file = hdf5::File::open(path).map_err(|e| Error::Hdf5 {
            path: path.to_path_buf(),
            detail: format!("cannot open file: {e}"),
        })?;

        let train = file.dataset("train").map_err(|e| Error::Hdf5 {
            path: path.to_path_buf(),
            detail: format!("missing 'train' dataset: {e}"),
        })?;
        let test = file.dataset("test").map_err(|e| Error::Hdf5 {
            path: path.to_path_buf(),
            detail: format!("missing 'test' dataset: {e}"),
        })?;

        let train_shape = train.shape();
        let test_shape = test.shape();

        let has_neighbors = file.dataset("neighbors").is_ok();
        let has_distances = file.dataset("distances").is_ok();

        let gt_k = if has_neighbors {
            let n = file.dataset("neighbors").unwrap();
            let s = n.shape();
            if s.len() == 2 {
                Some(s[1] as u32)
            } else {
                None
            }
        } else {
            None
        };

        let distance_attr = file
            .attr("distance")
            .ok()
            .and_then(|a| a.read_scalar::<hdf5::types::VarLenUnicode>().ok())
            .map(|s| s.to_string());

        Ok(DatasetInfo {
            num_train: train_shape[0] as u64,
            num_test: test_shape[0] as u64,
            dimension: train_shape[1] as u32,
            has_neighbors,
            has_distances,
            ground_truth_k: gt_k,
            distance_attr,
            file_size_bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        })
    }
}

pub struct DatasetInfo {
    pub num_train: u64,
    pub num_test: u64,
    pub dimension: u32,
    pub has_neighbors: bool,
    pub has_distances: bool,
    pub ground_truth_k: Option<u32>,
    pub distance_attr: Option<String>,
    pub file_size_bytes: u64,
}

impl std::fmt::Display for DatasetInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let size_mb = self.file_size_bytes as f64 / (1024.0 * 1024.0);
        let train_size_mb = self.num_train as f64 * self.dimension as f64 * 4.0 / (1024.0 * 1024.0);
        writeln!(
            f,
            "  Vectors:      {:>12} ({:.0} MB as f32)",
            self.num_train, train_size_mb
        )?;
        writeln!(f, "  Queries:      {:>12}", self.num_test)?;
        writeln!(f, "  Dimension:    {:>12}", self.dimension)?;
        writeln!(
            f,
            "  Distance:     {:>12}",
            self.distance_attr.as_deref().unwrap_or("(not set)")
        )?;
        writeln!(
            f,
            "  Ground truth: {:>12}",
            if self.has_neighbors {
                format!("yes (k={})", self.ground_truth_k.unwrap_or(0))
            } else {
                "no".to_string()
            }
        )?;
        write!(f, "  File size:    {:>9.1} MB", size_mb)
    }
}

struct ChunkIter {
    path: PathBuf,
    dim: u32,
    _start: u64,
    end: u64,
    chunk_size: usize,
    pos: u64,
}

impl Iterator for ChunkIter {
    type Item = Result<VectorChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.end {
            return None;
        }

        let chunk_end = (self.pos + self.chunk_size as u64).min(self.end);
        let n_rows = (chunk_end - self.pos) as usize;

        let result = (|| -> Result<VectorChunk> {
            let file = hdf5::File::open(&self.path).map_err(|e| Error::Hdf5 {
                path: self.path.clone(),
                detail: format!("cannot reopen file: {e}"),
            })?;
            let train = file.dataset("train").map_err(|e| Error::Hdf5 {
                path: self.path.clone(),
                detail: format!("cannot open 'train': {e}"),
            })?;

            let selection = ndarray::s![self.pos as usize..chunk_end as usize, ..];
            let data: Array2<f32> = train.read_slice_2d(selection).map_err(|e| Error::Hdf5 {
                path: self.path.clone(),
                detail: format!("failed to read rows {}..{}: {e}", self.pos, chunk_end),
            })?;

            assert_eq!(data.shape(), &[n_rows, self.dim as usize]);

            Ok(VectorChunk {
                start_index: self.pos,
                vectors: data,
            })
        })();

        self.pos = chunk_end;

        Some(result)
    }
}

impl DatasetReader for Hdf5Reader {
    fn meta(&self) -> &DatasetMeta {
        &self.meta
    }

    fn train_chunks(
        &self,
        chunk_size: usize,
    ) -> Box<dyn Iterator<Item = Result<VectorChunk>> + '_> {
        Box::new(ChunkIter {
            path: self.path.clone(),
            dim: self.meta.dimension,
            _start: 0,
            end: self.meta.num_vectors,
            chunk_size,
            pos: 0,
        })
    }

    fn train_range(
        &self,
        start: u64,
        end: u64,
        chunk_size: usize,
    ) -> Box<dyn Iterator<Item = Result<VectorChunk>> + '_> {
        let end = end.min(self.meta.num_vectors);
        Box::new(ChunkIter {
            path: self.path.clone(),
            dim: self.meta.dimension,
            _start: start,
            end,
            chunk_size,
            pos: start,
        })
    }

    fn queries(&self) -> Result<Array2<f32>> {
        let file = hdf5::File::open(&self.path).map_err(|e| Error::Hdf5 {
            path: self.path.clone(),
            detail: format!("cannot open file: {e}"),
        })?;
        let test = file.dataset("test").map_err(|e| Error::Hdf5 {
            path: self.path.clone(),
            detail: format!("missing 'test' dataset: {e}"),
        })?;
        test.read_2d().map_err(|e| Error::Hdf5 {
            path: self.path.clone(),
            detail: format!("failed to read 'test': {e}"),
        })
    }

    fn ground_truth(&self) -> Result<Array2<i64>> {
        let file = hdf5::File::open(&self.path).map_err(|e| Error::Hdf5 {
            path: self.path.clone(),
            detail: format!("cannot open file: {e}"),
        })?;
        let neighbors = file.dataset("neighbors").map_err(|e| Error::Hdf5 {
            path: self.path.clone(),
            detail: format!("missing 'neighbors' dataset (no ground truth): {e}"),
        })?;

        let raw: Array2<i32> = neighbors.read_2d().map_err(|e| Error::Hdf5 {
            path: self.path.clone(),
            detail: format!("failed to read 'neighbors': {e}"),
        })?;

        Ok(raw.mapv(|v| v as i64))
    }
}
