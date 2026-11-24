#!/usr/bin/env python3
"""Stream ParadeDB's published MS MARCO vectors into Rekall's HDF5 format."""

import argparse
import os
from pathlib import Path
from tempfile import NamedTemporaryFile

import h5py
import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.fs as fs
import pyarrow.parquet as pq

SOURCE = "paradedb-benchmark/datasets/msmarco/v1"
CORPUS = "sampled/113m/parquet/msmarco"
ROWS = 113_520_750
DIM = 1024
QUERIES = 100
BATCH = 4096


def vectors(column, dim):
    if column.null_count or not pc.all(pc.equal(pc.list_value_length(column), dim)).as_py():
        raise ValueError(f"Expected non-null {dim}-element embeddings")
    flat = pc.list_flatten(column)
    if flat.null_count:
        raise ValueError("Embedding contains a null coordinate")
    return flat.to_numpy().reshape(-1, dim)


def read_queries(filesystem, source, dim, count):
    def read(name, columns):
        return pq.read_table(f"{source}/queries/{name}.parquet",
                             columns=columns, filesystem=filesystem)

    # The blog's k6 export uses ORDER BY id LIMIT 100, not Parquet row order.
    queries = read("msmarco_queries", ["id", "emb"]).sort_by("id").slice(0, count)
    if len(queries) != count or pc.count_distinct(queries["id"]).as_py() != count:
        raise ValueError(f"Expected {count} distinct query IDs")
    truth = {}
    for k in (10, 100):
        table = read(f"ground_truth_knn_top{k}_unfiltered_113m", ["query_id", "gt_ids"])
        truth[k] = dict(zip(table["query_id"].to_pylist(), table["gt_ids"].to_pylist()))
        if len(truth[k]) != len(table):
            raise ValueError(f"Duplicate query ID in top-{k} ground truth")

    neighbors = []
    for query_id in queries["id"].to_pylist():
        top100 = truth[100].get(query_id, [])
        if (len(top100) != 100 or None in top100 or len(set(top100)) != 100
                or top100[:10] != truth[10].get(query_id)):
            raise ValueError(f"Missing, invalid or inconsistent ground truth for query {query_id}")
        neighbors.append(top100)
    return vectors(queries["emb"], dim), neighbors


def write_dataset(output, filesystem, source, rows, dim, count):
    queries, neighbors = read_queries(filesystem, source, dim, count)
    # Only hold the document-ID mapping needed by the held-out queries.
    wanted = sorted({doc for query in neighbors for doc in query})
    wanted_ids = pa.array(wanted, type=pa.large_string())
    row_ids = np.full(len(wanted), -1, dtype=np.int64)
    shards = sorted(
        entry.path for entry in filesystem.get_file_info(fs.FileSelector(f"{source}/{CORPUS}"))
        if entry.is_file and entry.path.endswith(".parquet")
    )
    if not shards:
        raise ValueError("No corpus shards found")

    with h5py.File(output, "w") as out:
        out.attrs["distance"] = "angular"
        train = out.create_dataset("train", (rows, dim), dtype="f4")
        offset = 0
        for number, shard in enumerate(shards, 1):
            with filesystem.open_input_file(shard) as handle:
                data = pq.ParquetFile(handle)
                for batch in data.iter_batches(batch_size=BATCH, columns=["_id", "emb"]):
                    end = offset + len(batch)
                    if end > rows:
                        raise ValueError(f"Corpus contains more than {rows:,} rows")
                    train[offset:end] = vectors(batch["emb"], dim)
                    matches = pc.index_in(batch["_id"], value_set=wanted_ids)
                    positions = pc.indices_nonzero(pc.is_valid(matches)).to_numpy()
                    for position, match in zip(positions, pc.drop_null(matches).to_numpy()):
                        if row_ids[match] != -1:
                            raise ValueError(f"Duplicate corpus ID: {wanted[match]}")
                        row_ids[match] = offset + position
                    offset = end
            if number % 50 == 0 or number == len(shards):
                print(f"{number}/{len(shards)} shards, {offset:,}/{rows:,} rows", flush=True)
        if offset != rows:
            raise ValueError(f"Expected {rows:,} corpus rows, found {offset:,}")
        missing = int(np.count_nonzero(row_ids == -1))
        if missing:
            raise ValueError(f"{missing:,} ground-truth document IDs not found in the corpus")
        ids = dict(zip(wanted, row_ids))
        out.create_dataset("test", data=queries, dtype="f4")
        out.create_dataset("neighbors", data=[[ids[doc] for doc in query] for query in neighbors],
                           dtype="i8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="Destination HDF5 file (about 433 GiB)")
    args = parser.parse_args()
    output = args.output.expanduser()
    if output.exists():
        parser.error(f"{output} already exists")
    output.parent.mkdir(parents=True, exist_ok=True)
    filesystem = fs.S3FileSystem(anonymous=True, region="us-east-1")
    with NamedTemporaryFile(dir=output.parent, prefix=f".{output.name}.", suffix=".partial") as tmp:
        write_dataset(tmp.name, filesystem, SOURCE, ROWS, DIM, QUERIES)
        # Publish only after the file closes and all corpus IDs are validated.
        os.link(tmp.name, output)
    print(f"Wrote {output}")


if __name__ == "__main__":
    main()
