use std::path::{Path, PathBuf};

use ndarray::Array2;

use crate::config::Metric;
use crate::error::{Error, Result};

use super::{DatasetMeta, DatasetReader, SourcePrecision, VectorChunk};

pub struct Hdf5Reader {
    path: PathBuf,
    meta: DatasetMeta,
}

/// Read the float width of a dataset's element type.
///
/// `to_descriptor` maps `H5Tget_class`/`H5Tget_size`, so this is a header
/// read -- no data is touched. The `f16` crate feature is what makes
/// `Float(U2)` representable at all; without it `to_descriptor` rejects a
/// 2-byte float outright, which is why enabling it is load-bearing here and
/// not cosmetic.
///
/// Anything that is not a float (an integer or fixed-point dataset) is
/// reported as f32: HDF5 still converts it to `f32` on read, and picking a
/// narrower heap column off a non-float source is not a call to make
/// implicitly.
fn element_precision(path: &Path, ds: &hdf5::Dataset) -> Result<SourcePrecision> {
    use hdf5::types::{FloatSize, TypeDescriptor};

    let dtype = ds.dtype().map_err(|e| Error::Hdf5 {
        path: path.to_path_buf(),
        detail: format!("cannot read element type: {e}"),
    })?;
    let descriptor = dtype.to_descriptor().map_err(|e| Error::Hdf5 {
        path: path.to_path_buf(),
        detail: format!("cannot interpret element type: {e}"),
    })?;

    Ok(match descriptor {
        TypeDescriptor::Float(FloatSize::U2) => SourcePrecision::F16,
        TypeDescriptor::Float(FloatSize::U8) => SourcePrecision::F64,
        _ => SourcePrecision::F32,
    })
}

/// Probe just the source precision, without the shape/dimension validation
/// `Hdf5Reader::open` does.
///
/// Init needs this before it can pick a column type, and init runs before
/// anything has validated the file.
pub fn source_precision(path: &Path) -> Result<SourcePrecision> {
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
    element_precision(path, &train)
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
                source_precision: element_precision(path, &train)?,
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
            source_precision: element_precision(path, &train)?,
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
    pub source_precision: SourcePrecision,
    pub file_size_bytes: u64,
}

impl std::fmt::Display for DatasetInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let size_mb = self.file_size_bytes as f64 / (1024.0 * 1024.0);
        let elem_bytes = f64::from(self.source_precision.bits() / 8);
        let train_size_mb =
            self.num_train as f64 * self.dimension as f64 * elem_bytes / (1024.0 * 1024.0);
        writeln!(
            f,
            "  Vectors:      {:>12} ({:.0} MB as {})",
            self.num_train, train_size_mb, self.source_precision
        )?;
        writeln!(f, "  Queries:      {:>12}", self.num_test)?;
        writeln!(f, "  Dimension:    {:>12}", self.dimension)?;
        writeln!(
            f,
            "  Precision:    {:>12}  (read as f32; heap column defaults to {})",
            self.source_precision.to_string(),
            if matches!(self.source_precision, SourcePrecision::F16) {
                "halfvec"
            } else {
                "vector"
            }
        )?;
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

    fn ground_truth_distances(&self) -> Result<Option<Array2<f32>>> {
        let file = hdf5::File::open(&self.path).map_err(|e| Error::Hdf5 {
            path: self.path.clone(),
            detail: format!("cannot open file: {e}"),
        })?;
        match file.dataset("distances") {
            Ok(ds) => {
                let dists: Array2<f32> = ds.read_2d().map_err(|e| Error::Hdf5 {
                    path: self.path.clone(),
                    detail: format!("failed to read 'distances': {e}"),
                })?;
                Ok(Some(dists))
            }
            Err(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{StorageKind, StorageSetting};
    use half::f16;
    use ndarray::Array2;

    /// Write a 2-d `train`/`test` pair in the requested element type.
    ///
    /// The file is closed, with every object in it, before the path is
    /// handed back: the writer holds HDF5's exclusive file lock, and a
    /// read-only open of the same path fails while it is held.
    fn write<T>(dir: &Path, name: &str, fill: impl Fn(usize) -> T) -> PathBuf
    where
        T: hdf5::H5Type + Clone + Default,
    {
        use hdf5::plist::file_access::FileCloseDegree;

        let path = dir.join(name);
        let file = hdf5::File::with_options()
            .with_fapl(|fapl| fapl.fclose_degree(FileCloseDegree::Strong))
            .create(&path)
            .unwrap();
        for (ds, rows) in [("train", 4usize), ("test", 2usize)] {
            let data =
                Array2::from_shape_fn((rows, DIM as usize), |(r, c)| fill(r * DIM as usize + c));
            file.new_dataset::<T>()
                .shape([rows, DIM as usize])
                .create(ds)
                .unwrap()
                .write(&data)
                .unwrap();
        }
        file.close().unwrap();
        path
    }

    const DIM: u32 = 3;

    #[test]
    fn source_precision_reflects_the_file_not_what_we_read() {
        let dir = tempfile::tempdir().unwrap();

        // The f16 case is the one the `f16` cargo feature buys: without it
        // `to_descriptor` rejects a 2-byte float and this returns an error
        // instead of F16.
        let f16_path = write::<f16>(dir.path(), "half.hdf5", |i| f16::from_f32(i as f32));
        assert_eq!(
            source_precision(&f16_path).unwrap(),
            SourcePrecision::F16,
            "2-byte float misread"
        );

        let f32_path = write::<f32>(dir.path(), "single.hdf5", |i| i as f32);
        assert_eq!(source_precision(&f32_path).unwrap(), SourcePrecision::F32);

        let f64_path = write::<f64>(dir.path(), "double.hdf5", |i| i as f64);
        assert_eq!(source_precision(&f64_path).unwrap(), SourcePrecision::F64);

        // Whatever the file holds, the reader still hands out f32 -- HDF5
        // converts inside H5Dread -- so precision is a storage decision, not
        // a decoding one.
        for path in [&f16_path, &f32_path, &f64_path] {
            let reader = Hdf5Reader::open(path, crate::config::Metric::Angular, DIM).unwrap();
            let q: Array2<f32> = reader.queries().unwrap();
            assert_eq!(q.shape(), &[2, DIM as usize]);
            assert_eq!(q[[0, 0]], 0.0);
            assert_eq!(q[[0, 1]], 1.0);
        }
    }

    #[test]
    fn auto_storage_matches_the_source_and_never_widens_f16() {
        // Auto: halfvec only for an f16 source. Widening f16 into `vector`
        // stores bit-identical values in twice the space, so it must not be
        // what a default does.
        assert_eq!(
            StorageSetting::Auto.resolve(SourcePrecision::F16),
            StorageKind::Halfvec
        );
        assert_eq!(
            StorageSetting::Auto.resolve(SourcePrecision::F32),
            StorageKind::Vector
        );
        // f64 narrows to f32 on read; going further to f16 is a real
        // tradeoff, so auto stops at vector.
        assert_eq!(
            StorageSetting::Auto.resolve(SourcePrecision::F64),
            StorageKind::Vector
        );

        // Explicit settings ignore the source entirely.
        for source in [
            SourcePrecision::F16,
            SourcePrecision::F32,
            SourcePrecision::F64,
        ] {
            assert_eq!(
                StorageSetting::Vector.resolve(source),
                StorageKind::Vector,
                "explicit vector overridden by {source}"
            );
            assert_eq!(
                StorageSetting::Halfvec.resolve(source),
                StorageKind::Halfvec,
                "explicit halfvec overridden by {source}"
            );
        }

        // Only an explicit narrowing takes a suffix: it is the one that needs
        // to coexist with the natural table. Auto never does, so table names
        // stay computable without opening the dataset.
        assert_eq!(StorageSetting::Auto.table_suffix(), "");
        assert_eq!(StorageSetting::Vector.table_suffix(), "");
        assert_eq!(StorageSetting::Halfvec.table_suffix(), "_halfvec");
    }
}
