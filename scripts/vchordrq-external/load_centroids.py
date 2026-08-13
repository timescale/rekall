#!/usr/bin/env python3
# Load externally computed K-means centroids (a .npy from train.py) into
# the centroid table VectorChord's [build.external] expects:
#   (id integer, parent integer, vector vector(dim))
# with row 0 = the mean of all centroids as the root, children parented
# to it -- the exact layout of VectorChord's original scripts/index.py
# (removed upstream in their #420; train.py here is vendored verbatim
# from that history, TensorChord's dual AGPLv3/ELv2 license preserved).
#
# Usage:
#   python load_centroids.py --centroids centroid.npy --table gist_960_centroids \
#       --dim 960 --dsn "host=/var/run/postgresql dbname=postgres"
#
# Run between `rekall load` and `rekall index`; point the index arm's
# `external_build_table` param at the created table.
import argparse

import numpy as np
import psycopg


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--centroids", required=True, help="centroid .npy from train.py")
    ap.add_argument("--table", required=True, help="centroid table to (re)create")
    ap.add_argument("--dim", type=int, required=True)
    ap.add_argument("--dsn", required=True)
    args = ap.parse_args()

    centroids = np.load(args.centroids)
    assert centroids.ndim == 2 and centroids.shape[1] == args.dim, centroids.shape
    root = np.mean(centroids, axis=0)

    with psycopg.connect(args.dsn, autocommit=True) as conn:
        cur = conn.cursor()
        cur.execute(f"DROP TABLE IF EXISTS {args.table}")
        cur.execute(
            f"CREATE TABLE {args.table} "
            f"(id integer, parent integer, vector vector({args.dim}))"
        )
        with cur.copy(
            f"COPY {args.table} (id, parent, vector) FROM STDIN"
        ) as copy:
            def row(i, parent, v):
                vec = "[" + ",".join("%.8g" % x for x in v) + "]"
                copy.write_row((i, parent, vec))

            row(0, None, root)
            for i, c in enumerate(centroids):
                row(i + 1, 0, c)
    print(f"loaded {len(centroids)} centroids (+root) into {args.table}")


if __name__ == "__main__":
    main()
