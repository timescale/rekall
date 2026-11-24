// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

//! Native reader for ParadeDB's public MS MARCO Parquet fixtures.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use arrow_array::{
    Array, FixedSizeListArray, Float32Array, Float64Array, Int32Array, Int64Array, LargeListArray,
    LargeStringArray, ListArray, RecordBatch, StringArray, StringViewArray, UInt32Array,
    UInt64Array,
};
use aws_sdk_s3::{config::Region, Client};
use indicatif::{ProgressBar, ProgressStyle};
use ndarray::Array2;
use parquet::arrow::{
    arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder},
    ProjectionMask,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWriteExt, BufWriter};

use super::{DatasetMeta, DatasetReader, SourcePrecision, VectorChunk};
use crate::{config::Metric, error::Error};

const BUCKET: &str = "paradedb-benchmark";
const PREFIX: &str = "datasets/msmarco/v1";
const CORPUS: &str = "sampled/113m/parquet/msmarco/";
const ROWS: usize = 113_520_750;
pub(crate) const DIM: usize = 1024;
const QUERIES: usize = 100;
const BATCH: usize = 4096;
const MANIFEST: &str = "rekall-msmarco-v1.json";

/// The manifest is published only after the shards and ground truth are ready.
pub(crate) fn is_ready(dir: &Path) -> bool {
    dir.join(MANIFEST).is_file()
}

/// Cache the source files and map ground-truth IDs to corpus row numbers.
pub(crate) async fn prepare(dest: &Path) -> crate::error::Result<()> {
    let display = dest.display().to_string();
    let dest = dest.to_path_buf();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if is_ready(&dest) {
            return Ok(());
        }
        std::fs::create_dir_all(&dest)?;
        let client = Client::from_conf(
            aws_sdk_s3::Config::builder()
                .behavior_version_latest()
                .region(Region::new("us-east-1"))
                .allow_no_auth()
                .build(),
        );
        let mut keys = runtime.block_on(list_shards(&client))?;
        keys.sort_unstable();
        let progress = ProgressBar::new(keys.len() as u64);
        progress.set_style(ProgressStyle::with_template(
            "Caching MS MARCO [{wide_bar}] {pos}/{len} shards",
        )?);
        let cache = |key: &str| -> Result<File> {
            let path = object_path(&dest, key)?;
            if !path.is_file() {
                let parent = path.parent().context("missing cache directory")?;
                std::fs::create_dir_all(parent)?;
                let tmp = tempfile::NamedTempFile::new_in(parent)?;
                runtime
                    .block_on(fetch(&client, key, tmp.reopen()?))
                    .with_context(|| format!("downloading s3://{BUCKET}/{key}"))?;
                tmp.persist(&path)?;
            }
            Ok(File::open(path)?)
        };
        let manifest = build_manifest(&keys, (ROWS, DIM, QUERIES), cache, || progress.inc(1))?;
        save_manifest(&dest, &manifest)?;
        progress.finish_and_clear();
        Ok(())
    })
    .await
    .context("preparation worker stopped")
    .and_then(|result| result)
    .map_err(|e| {
        crate::error::Error::Config(format!("Cannot prepare MS MARCO at '{display}': {e:#}"))
    })
}

async fn list_shards(client: &Client) -> Result<Vec<String>> {
    let prefix = format!("{PREFIX}/{CORPUS}");
    let mut pages = client
        .list_objects_v2()
        .bucket(BUCKET)
        .prefix(&prefix)
        .into_paginator()
        .send();
    let mut keys = Vec::new();
    while let Some(page) = pages.next().await {
        keys.extend(
            page.context("listing public MS MARCO corpus")?
                .contents()
                .iter()
                .filter_map(|object| object.key())
                .filter(|key| {
                    key.strip_prefix(&prefix)
                        .is_some_and(|name| name.ends_with(".parquet") && !name.contains('/'))
                })
                .map(str::to_owned),
        );
    }
    ensure!(!keys.is_empty(), "no corpus shards found");
    Ok(keys)
}

async fn fetch(client: &Client, key: &str, target: File) -> Result<()> {
    let response = client.get_object().bucket(BUCKET).key(key).send().await?;
    let mut file = BufWriter::with_capacity(8 * 1024 * 1024, tokio::fs::File::from_std(target));
    let copied = tokio::io::copy(&mut response.body.into_async_read(), &mut file).await?;
    file.flush().await?;
    if let Some(expected) = response.content_length {
        ensure!(
            copied as i64 == expected,
            "downloaded {copied} of {expected} bytes"
        );
    }
    Ok(())
}

fn object_path(dir: &Path, key: &str) -> Result<PathBuf> {
    let path = Path::new(key);
    ensure!(
        !key.is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "invalid Parquet object key '{key}'"
    );
    Ok(dir.join(path))
}

fn projected(file: File, names: &[&str]) -> Result<ParquetRecordBatchReaderBuilder<File>> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let mask = ProjectionMask::columns(builder.parquet_schema(), names.iter().copied());
    Ok(builder.with_projection(mask))
}

fn scan(
    fetch: &mut impl FnMut(&str) -> Result<File>,
    key: &str,
    names: &[&str],
    mut visit: impl FnMut(RecordBatch) -> Result<()>,
) -> Result<()> {
    (|| -> Result<()> {
        let reader = projected(fetch(key)?, names)?
            .with_batch_size(BATCH)
            .build()?;
        for batch in reader {
            visit(batch?)?;
        }
        Ok(())
    })()
    .with_context(|| format!("reading '{key}'"))
}

fn column<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a dyn Array> {
    batch
        .column_by_name(name)
        .map(|column| column.as_ref())
        .with_context(|| format!("missing column '{name}'"))
}

fn text(array: &dyn Array, row: usize) -> Result<&str> {
    ensure!(!array.is_null(row), "null document or query ID");
    let array = array.as_any();
    if let Some(strings) = array.downcast_ref::<StringArray>() {
        Ok(strings.value(row))
    } else if let Some(strings) = array.downcast_ref::<LargeStringArray>() {
        Ok(strings.value(row))
    } else if let Some(strings) = array.downcast_ref::<StringViewArray>() {
        Ok(strings.value(row))
    } else {
        bail!("expected string IDs")
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum QueryId {
    Number(i128),
    Text(String),
}

fn query_id(array: &dyn Array, row: usize) -> Result<QueryId> {
    ensure!(!array.is_null(row), "null query ID");
    let typed = array.as_any();
    macro_rules! number {
        ($ty:ty) => {
            if let Some(values) = typed.downcast_ref::<$ty>() {
                return Ok(QueryId::Number(values.value(row).into()));
            }
        };
    }
    number!(Int32Array);
    number!(Int64Array);
    number!(UInt32Array);
    number!(UInt64Array);
    Ok(QueryId::Text(text(array, row)?.to_owned()))
}

fn list(array: &dyn Array, row: usize) -> Result<(&dyn Array, std::ops::Range<usize>)> {
    ensure!(!array.is_null(row), "null embedding or neighbor list");
    let array = array.as_any();
    if let Some(list) = array.downcast_ref::<ListArray>() {
        let offsets = list.value_offsets();
        Ok((
            list.values().as_ref(),
            offsets[row] as usize..offsets[row + 1] as usize,
        ))
    } else if let Some(list) = array.downcast_ref::<LargeListArray>() {
        let offsets = list.value_offsets();
        Ok((
            list.values().as_ref(),
            offsets[row] as usize..offsets[row + 1] as usize,
        ))
    } else if let Some(list) = array.downcast_ref::<FixedSizeListArray>() {
        let start = list.value_offset_at(row);
        Ok((
            list.values().as_ref(),
            start..start + list.value_length() as usize,
        ))
    } else {
        bail!("expected a list column")
    }
}

fn append_embedding(array: &dyn Array, row: usize, dim: usize, out: &mut Vec<f32>) -> Result<()> {
    let (values, range) = list(array, row)?;
    ensure!(range.len() == dim, "expected {dim}-element embedding");
    if values.null_count() != 0 {
        ensure!(
            range.clone().all(|i| values.is_valid(i)),
            "null embedding coordinate"
        );
    }
    if let Some(values) = values.as_any().downcast_ref::<Float32Array>() {
        out.extend_from_slice(&values.values()[range]);
    } else if let Some(values) = values.as_any().downcast_ref::<Float64Array>() {
        out.extend(values.values()[range].iter().map(|&v| v as f32));
    } else {
        bail!("expected floating-point embeddings")
    }
    Ok(())
}

#[derive(Default)]
struct Query {
    vector: Vec<f32>,
    top10: Vec<String>,
    top100: Vec<String>,
}

fn read_queries(
    fetch: &mut impl FnMut(&str) -> Result<File>,
    dim: usize,
    count: usize,
) -> Result<BTreeMap<QueryId, Query>> {
    let mut queries = BTreeMap::new();
    scan(
        fetch,
        &format!("{PREFIX}/queries/msmarco_queries.parquet"),
        &["id", "emb"],
        |batch| {
            let ids = column(&batch, "id")?;
            let embeddings = column(&batch, "emb")?;
            for row in 0..batch.num_rows() {
                let id = query_id(ids, row)?;
                if queries.len() == count
                    && queries.last_key_value().is_some_and(|(last, _)| &id > last)
                {
                    continue;
                }
                let mut query = Query::default();
                append_embedding(embeddings, row, dim, &mut query.vector)?;
                ensure!(
                    queries.insert(id, query).is_none(),
                    "duplicate selected query ID"
                );
                if queries.len() > count {
                    queries.pop_last();
                }
            }
            Ok(())
        },
    )?;
    ensure!(
        queries.len() == count,
        "expected {count} distinct query IDs"
    );

    for k in [10, 100] {
        let key = format!("{PREFIX}/queries/ground_truth_knn_top{k}_unfiltered_113m.parquet");
        scan(fetch, &key, &["query_id", "gt_ids"], |batch| {
            let ids = column(&batch, "query_id")?;
            let neighbors = column(&batch, "gt_ids")?;
            for row in 0..batch.num_rows() {
                let Some(query) = queries.get_mut(&query_id(ids, row)?) else {
                    continue;
                };
                let target = if k == 10 {
                    &mut query.top10
                } else {
                    &mut query.top100
                };
                ensure!(target.is_empty(), "duplicate top-{k} query ID");
                let (values, range) = list(neighbors, row)?;
                ensure!(range.len() == k, "expected {k} neighbors");
                *target = range
                    .map(|i| text(values, i).map(str::to_owned))
                    .collect::<Result<_>>()?;
            }
            Ok(())
        })?;
    }
    for query in queries.values() {
        ensure!(
            query.top10.len() == 10
                && query.top100.len() == 100
                && query.top100.starts_with(&query.top10)
                && query.top100.iter().collect::<HashSet<_>>().len() == 100,
            "missing or inconsistent ground truth"
        );
    }
    Ok(queries)
}

#[derive(Debug, Serialize, Deserialize)]
struct Shard {
    key: String,
    rows: u64,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    meta: DatasetMeta,
    shards: Vec<Shard>,
    queries: Vec<f32>,
    neighbors: Vec<i64>,
}

fn build_manifest(
    shards: &[String],
    (rows, dim, count): (usize, usize, usize),
    mut fetch: impl FnMut(&str) -> Result<File>,
    mut on_shard: impl FnMut(),
) -> Result<Manifest> {
    let queries = read_queries(&mut fetch, dim, count)?;
    // Only map the corpus IDs referenced by the selected ground truth.
    let mut row_ids: HashMap<&str, Option<i64>> = queries
        .values()
        .flat_map(|query| query.top100.iter().map(|id| (id.as_str(), None)))
        .collect();
    let mut offset = 0usize;
    let mut cached = Vec::with_capacity(shards.len());
    for key in shards {
        let start = offset;
        scan(&mut fetch, key, &["_id"], |batch| {
            let end = offset + batch.num_rows();
            ensure!(end <= rows, "corpus contains more than {rows} rows");
            let ids = column(&batch, "_id")?;
            for row in 0..batch.num_rows() {
                if let Some(id) = row_ids.get_mut(text(ids, row)?) {
                    ensure!(
                        id.is_none(),
                        "duplicate corpus ID referenced by ground truth"
                    );
                    *id = Some((offset + row) as i64);
                }
            }
            offset = end;
            Ok(())
        })?;
        cached.push(Shard {
            key: key.clone(),
            rows: (offset - start) as u64,
        });
        on_shard();
    }
    ensure!(
        offset == rows,
        "expected {rows} corpus rows, found {offset}"
    );
    let mut test = Vec::with_capacity(count * dim);
    let mut neighbors = Vec::with_capacity(count * 100);
    for query in queries.values() {
        test.extend_from_slice(&query.vector);
        for id in &query.top100 {
            neighbors.push(
                row_ids[id.as_str()]
                    .with_context(|| format!("ground-truth ID '{id}' missing from corpus"))?,
            );
        }
    }
    Ok(Manifest {
        meta: DatasetMeta {
            num_vectors: rows as u64,
            dimension: dim as u32,
            num_queries: count as u64,
            metric: Metric::Angular,
            source_precision: SourcePrecision::F32,
        },
        shards: cached,
        queries: test,
        neighbors,
    })
}

fn save_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(tmp.as_file_mut(), manifest)?;
    tmp.persist(dir.join(MANIFEST))?;
    Ok(())
}

fn dataset_error(path: &Path, error: anyhow::Error) -> Error {
    Error::Config(format!(
        "Cannot read MS MARCO Parquet at '{}': {error:#}",
        path.display()
    ))
}

pub(crate) struct Reader {
    dir: PathBuf,
    manifest: Manifest,
}

impl Reader {
    pub(crate) fn open(dir: &Path) -> crate::error::Result<Self> {
        let manifest = File::open(dir.join(MANIFEST))
            .context("opening dataset manifest")
            .and_then(|file| serde_json::from_reader(file).context("parsing dataset manifest"))
            .map_err(|err| dataset_error(dir, err))?;
        Ok(Self {
            dir: dir.to_owned(),
            manifest,
        })
    }
}

impl DatasetReader for Reader {
    fn meta(&self) -> &DatasetMeta {
        &self.manifest.meta
    }

    fn train_chunks(
        &self,
        chunk_size: usize,
    ) -> Box<dyn Iterator<Item = crate::error::Result<VectorChunk>> + '_> {
        self.train_range(0, self.meta().num_vectors, chunk_size)
    }

    fn train_range(
        &self,
        start: u64,
        end: u64,
        chunk_size: usize,
    ) -> Box<dyn Iterator<Item = crate::error::Result<VectorChunk>> + '_> {
        let end = end.min(self.meta().num_vectors);
        let mut shard_start = 0;
        let mut pos = start;
        let mut shards = self.manifest.shards.iter();
        let mut batches: Option<ParquetRecordBatchReader> = None;
        let read = move || -> Result<Option<VectorChunk>> {
            while pos < end {
                if let Some(batch) = batches.as_mut().and_then(Iterator::next) {
                    let batch = batch?;
                    let dim = self.meta().dimension as usize;
                    let embeddings = column(&batch, "emb")?;
                    let mut vectors = Vec::with_capacity(batch.num_rows() * dim);
                    for row in 0..batch.num_rows() {
                        append_embedding(embeddings, row, dim, &mut vectors)?;
                    }
                    let chunk = VectorChunk {
                        start_index: pos,
                        vectors: Array2::from_shape_vec((batch.num_rows(), dim), vectors)?,
                    };
                    pos += batch.num_rows() as u64;
                    return Ok(Some(chunk));
                }
                let shard = shards
                    .next()
                    .context("manifest ended before the requested training range")?;
                let begin = shard_start;
                shard_start += shard.rows;
                if pos >= shard_start {
                    continue;
                }
                ensure!(pos >= begin, "shard ended before its declared row count");
                let file = File::open(object_path(&self.dir, &shard.key)?)
                    .with_context(|| format!("opening shard '{}'", shard.key))?;
                let builder = projected(file, &["emb"])?;
                ensure!(
                    builder.metadata().file_metadata().num_rows() as u64 == shard.rows,
                    "shard '{}' no longer has {} rows",
                    shard.key,
                    shard.rows
                );
                batches = Some(
                    builder
                        .with_offset((pos - begin) as usize)
                        .with_limit((end.min(shard_start) - pos) as usize)
                        .with_batch_size(chunk_size.clamp(1, BATCH))
                        .build()?,
                );
            }
            Ok(None)
        };
        let mut read = read;
        let mut failed = false;
        Box::new(std::iter::from_fn(move || {
            if failed {
                return None;
            }
            match read() {
                Ok(chunk) => chunk.map(Ok),
                Err(err) => {
                    failed = true;
                    Some(Err(dataset_error(&self.dir, err)))
                }
            }
        }))
    }

    fn queries(&self) -> crate::error::Result<Array2<f32>> {
        Array2::from_shape_vec(
            (
                self.meta().num_queries as usize,
                self.meta().dimension as usize,
            ),
            self.manifest.queries.clone(),
        )
        .map_err(|err| dataset_error(&self.dir, err.into()))
    }

    fn ground_truth(&self) -> crate::error::Result<Array2<i64>> {
        Array2::from_shape_vec(
            (self.meta().num_queries as usize, 100),
            self.manifest.neighbors.clone(),
        )
        .map_err(|err| dataset_error(&self.dir, err.into()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{
        builder::{Float32Builder, ListBuilder, StringBuilder},
        ArrayRef,
    };
    use parquet::arrow::ArrowWriter;

    use super::*;

    fn embeddings(values: impl IntoIterator<Item = f32>) -> ArrayRef {
        let mut list = ListBuilder::new(Float32Builder::new());
        for value in values {
            list.values().append_slice(&[value, value + 0.5]);
            list.append(true);
        }
        Arc::new(list.finish())
    }

    fn write(dir: &Path, key: &str, columns: Vec<(&str, ArrayRef)>) {
        let path = object_path(dir, key).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let batch = RecordBatch::try_from_iter(columns).unwrap();
        let mut writer =
            ArrowWriter::try_new(File::create(path).unwrap(), batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    fn fixtures(dir: &Path, missing_id: bool) -> Vec<String> {
        let shards = [
            "corpus/000.parquet".to_owned(),
            "corpus/001.parquet".to_owned(),
        ];
        for (key, rows) in shards.iter().zip([0..49, 49..100]) {
            write(
                dir,
                key,
                vec![
                    (
                        "_id",
                        Arc::new(StringArray::from_iter_values(
                            rows.clone().map(|id| format!("doc-{id}")),
                        )),
                    ),
                    ("emb", embeddings(rows.map(|id| id as f32))),
                ],
            );
        }
        write(
            dir,
            &format!("{PREFIX}/queries/msmarco_queries.parquet"),
            vec![
                ("id", Arc::new(Int64Array::from(vec![10, 2, 1]))),
                ("emb", embeddings([10.0, 2.0, 1.0])),
            ],
        );
        for k in [10, 100] {
            let mut neighbors = ListBuilder::new(StringBuilder::new());
            for query in [2, 1] {
                for offset in 0..k {
                    let id = if query == 1 { 99 - offset } else { offset };
                    if missing_id && k == 100 && query == 1 && offset == 99 {
                        neighbors.values().append_value("missing-doc");
                    } else {
                        neighbors.values().append_value(format!("doc-{id}"));
                    }
                }
                neighbors.append(true);
            }
            write(
                dir,
                &format!("{PREFIX}/queries/ground_truth_knn_top{k}_unfiltered_113m.parquet"),
                vec![
                    ("query_id", Arc::new(Int64Array::from(vec![2, 1]))),
                    ("gt_ids", Arc::new(neighbors.finish())),
                ],
            );
        }
        shards.into()
    }

    #[test]
    fn native_reader_streams_ranges_and_matches_corpus_row_ids() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let shards = fixtures(dir, false);
        let fetch = |key: &str| Ok(File::open(object_path(dir, key)?)?);
        let manifest = build_manifest(&shards, (100, 2, 2), fetch, || {}).unwrap();
        assert!(!is_ready(dir));
        save_manifest(dir, &manifest).unwrap();
        assert!(is_ready(dir));
        let reader = Reader::open(dir).unwrap();
        assert_eq!(reader.meta().num_vectors, 100);
        assert_eq!(
            reader.queries().unwrap(),
            ndarray::array![[1.0, 1.5], [2.0, 2.5]]
        );
        let truth = reader.ground_truth().unwrap();
        assert_eq!(truth.row(0).to_vec(), (0i64..100).rev().collect::<Vec<_>>());
        assert_eq!(truth.row(1).to_vec(), (0i64..100).collect::<Vec<_>>());
        assert!(reader.ground_truth_distances().unwrap().is_none());
        let range = reader
            .train_range(48, 52, 2)
            .collect::<crate::error::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            range
                .iter()
                .map(|chunk| chunk.start_index)
                .collect::<Vec<_>>(),
            [48, 49, 51]
        );
        assert_eq!(
            range[1].vectors,
            ndarray::array![[49.0, 49.5], [50.0, 50.5]]
        );
        let full = reader
            .train_chunks(12)
            .collect::<crate::error::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            full.iter()
                .map(|chunk| chunk.vectors.nrows())
                .sum::<usize>(),
            100
        );
        assert!(full.iter().all(|chunk| chunk.vectors.nrows() <= 12));
        assert!(reader.train_range(100, 120, 2).next().is_none());
    }

    #[test]
    fn missing_ground_truth_document_does_not_publish_a_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let shards = fixtures(dir, true);
        let result = build_manifest(
            &shards,
            (100, 2, 2),
            |key| Ok(File::open(object_path(dir, key)?)?),
            || {},
        );
        assert!(result.err().unwrap().to_string().contains("'missing-doc'"));
        assert!(!is_ready(dir));
    }
}
