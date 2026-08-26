# Cohere-1M: what is the smallest instance Meerkat and HNSW can run on?

This is the sizing analysis behind `cohere-1m-mkt-vs-hnsw.toml` (the
reference run) and `cohere-1m-mkt-vs-hnsw.toml` (the memory ladder).

Every number labelled *measured* comes from the published rekall runs at
<https://timescale.github.io/rekall/> (each run exposes a `summary.json`
with a `builds[]` array of `size_bytes` / `build_time_secs`). Numbers
labelled *derived* are extrapolated from those, with the derivation shown.
Nothing here has been measured on a small instance yet — that is what the
ladder is for.

## Footprints at cohere-1M (1M rows x 768d)

| Object | Size | Source |
|---|---:|---|
| Heap (`cohere_1m`, `v vector(768)`) | ~4.10 GB | derived: `v` is `STORAGE PLAIN`, so a tuple is 24 B header + 8 B id + 3080 B vector = 3112 B; 2 tuples/page; 500 k pages x 8 kB. Corroborated by the measured 3.82 GiB IVFFlat full-vector index. |
| `mkt-defaults` | **0.28 GB** (267 MiB) | measured, cohere-1M 2026-08-11 |
| `hnsw-m24-halfvec` | **~2.05 GB** (1.91 GiB) | derived: measured 19.07 GiB @ 10M, 95.35 GiB @ 50M, 179.18 GiB @ 100M — dead linear at 1.91 GiB per 1M rows |
| `hnsw-halfvec` (m=16) | ~2.0 GB | derived: m=16 fullvec measures 3.74 GB/M (laion-5M) vs m=24 fullvec 3.81 GB/M (cohere) — only ~2% apart |
| `hnsw-m24` (fullvec) | ~4.10 GB | measured 38.14 GiB @ 10M, 190.69 GiB @ 50M |

The headline ratio: **Meerkat's index is ~7.3x smaller than the HNSW
halfvec graph** at this scale.

Two things follow that are easy to miss:

1. **Lowering `m` is not an escape route.** At 768d the vector payload
   dominates an HNSW node and the neighbour lists do not, so m=16 buys
   ~2%. If a 2 GB graph does not fit, no HNSW tuning makes it fit.
2. **The heap is the same 4.1 GB in both cases** and it is bigger than
   either index. Any claim of the form "Meerkat fits in X GB" is about the
   *index*, not about caching the whole table.

## Build sizing vs query sizing

These are different constraints and they bind on different engines.

### Build

pgvector's HNSW build has a **cliff**: it keeps the whole graph in
`maintenance_work_mem`, and when the graph outgrows it, the build falls
back to an on-disk pass that inserts the remainder one tuple at a time —
roughly an order of magnitude slower. The graph is ~2.05 GB and no HNSW
parameter shrinks it, so the floor is set by how much
`maintenance_work_mem` the box can be made to give up.

**With stock auto-tuning that floor is 8 GiB.** rekall derives
`maintenance_work_mem = 2*RAM/5` (`rekall-lib/src/infra/config.rs`), capped
there because pgvector's *parallel* build puts the graph in a DSM segment
and DSM comes out of `/dev/shm`, which is 50% of RAM. Solving
`2*RAM/5 >= 2 GB` gives RAM >= 5 GB, i.e. the 8 GiB rung.

**But that floor is an artifact of static tuning, and rekall can lift it.**
`[phase_pg_config]` (commit `acc62b8`) applies a GUC map via `ALTER SYSTEM`
for the duration of a single phase and restores the previous state
afterward — reloadable GUCs via `pg_reload_conf()`, postmaster-context ones
(`shared_buffers`) via a detected PostgreSQL restart, with the pre-phase
state reinstated even if the phase fails. `shared_buffers`
is dead weight during a build — the heap scan is sequential and the graph
lives in `maintenance_work_mem` — so the build phase can collapse it to
128MB and hand the difference over:

```toml
[phase_pg_config.index]
shared_buffers                   = "128MB"
maintenance_work_mem             = "2560MB"
max_parallel_maintenance_workers = "0"
```

The third setting is the one that is easy to miss. Raising
`maintenance_work_mem` past 50% of RAM is useless for a *parallel* build,
because the DSM segment it would need cannot exceed `/dev/shm`. Forcing the
build serial puts the graph in backend-local memory instead and removes
that ceiling — at the cost of giving up what parallelism the box had.

On the 4 GiB rung that budget is 128MB + 2560MB + ~500MB of OS and
PostgreSQL overhead ≈ 3.2 GB, which fits. So the **HNSW build floor is
4 GiB, not 8 GiB** — matching its query floor rather than exceeding it. At
2 GiB there is no arrangement that works: 2.5 GB of graph does not fit in
2 GiB of RAM, and the build can only complete via the spill path.
`cohere-1m-build-floor.toml` runs this experiment down the ladder.

Meerkat has **no build-memory cliff**. Its k-means sample buffer is
explicitly bounded by `maintenance_work_mem`
(`sample_for_build()` in `src/pg/mktann_build.c`): the ideal sample is
`nlist * 256` vectors, and when that does not fit the build subsamples and
recovers accuracy with a later page-backed refine pass over the full table.
Lower `maintenance_work_mem` costs some centroid quality, not a 10x build
time. At 1M with `nlist = 3906` the ideal sample is the entire table
(3.1 GB); with `maintenance_work_mem = 384 MB` it draws ~131 k vectors and
refines. So the Meerkat build floor is set by "does PostgreSQL start", not
by the index.

Build *time* is a separate practical limit. HNSW m24 halfvec at 10M took
2427 s on 16 Graviton vCPUs; the build is ~N log N and scales sublinearly
in workers, so 1M on 2 vCPU should land in the tens of minutes. Meerkat at
1M took 17.6 s on 96 vCPU (measured) and 796 s at 10M on 16 vCPU, so
minutes on 2 vCPU.

### Query

HNSW keeps the halfvec **inside** the graph, so an unfiltered top-k query
touches the heap only for the k rows it finally returns — its working set
is the ~2 GB graph and essentially nothing else.
Meerkat scans compressed posting lists and then reranks exactly against the
heap, so its working set is a 0.28 GB index **plus** a bounded random-read
tail: the auto rerank pool is `16 * k` = 160 tuples per query
(`mkt.rerank_pool`, `src/index/query_scan.c`).

That makes Meerkat look more I/O-dependent on paper, and the ladder needs
to test it honestly. But the two miss patterns are not comparable:

- An **HNSW walk is a chain of dependent reads**. Hop N+1's address is only
  known after hop N has been read, so a cache miss cannot be prefetched and
  its full latency lands on the query. A few hundred serialized ~100 µs gp3
  reads is tens of milliseconds per query.
- **Meerkat's rerank fetches are independent**. All 160 TIDs are known
  before the first fetch, and the rerank path drives them through
  PostgreSQL 18's `read_stream` API (`mkt_storage_rerank` in
  `src/pg/mktann_storage.c`), which batches them into concurrent async
  reads under `effective_io_concurrency` (200 on these boxes). The
  posting-list scan itself is near-sequential over contiguous pages.

So the expectation the ladder is testing is: Meerkat degrades from a small
resident index plus prefetchable misses; HNSW degrades from an unprefetchable
graph walk. `search.io_stats` records the per-point read counters that
either confirm or refute this.

## The ladder

All Graviton4, all 100 GB gp3 at its included baseline (3000 IOPS /
125 MiB/s), held constant. Provisioning more would cost ~$108/mo — more than
any instance on the ladder — and would dominate the cost figure being
measured; a real deployment on a $66/mo box runs gp3 defaults. Warm runs are
insensitive to it anyway wherever the working set fits: the 8 GiB rung
recorded zero disk reads per sweep point once its cache settled. The three `.large` rungs are all 2 vCPU, so RAM is
the only variable between them.

| Config | vCPU | RAM | `shared_buffers` | `maintenance_work_mem` |
|---|---:|---:|---:|---:|
| `infra/instances/m8g.large.toml` | 2 | 8 GiB | 2 GB | 3 GB |
| `infra/instances/c8g.large.toml` | 2 | 4 GiB | 1 GB | 1 GB |
| `infra/instances/c8g.medium.toml` | 1 | 2 GiB | 512 MB | 384 MB (explicit) |

`infra/instances/r8g.large.toml` (2 vCPU, 16 GiB) is kept as an optional
full-fidelity reference box but is not a rung — 16 GiB cannot be the
cheapest answer if 8 GiB clears the target.

`c8g.medium` needs every memory GUC set explicitly: the auto-tuner computes
`shared_buffers = RAM/4` in whole GB, which is `0GB` at 2 GiB and will not
start PostgreSQL.

`c8g.medium` is also **query-only**. `provision.sh` compiles meerkat (and
optionally rekall) from source, which does not fit in 2 GiB, and an HNSW
build there would spill for hours.

### Predictions

Written down before the run so the results can contradict them.

| Rung | Meerkat | HNSW halfvec |
|---|---|---|
| 16 GiB | index + heap cached | graph + heap cached |
| 8 GiB | index + heap cached (4.4 GB) | graph cached; heap mostly not, and queries do not need it |
| 4 GiB | index cached; ~25% of heap cached, rerank misses prefetchable | graph fits across `shared_buffers` + page cache, but nothing else does. **Both floors.** |
| 2 GiB | index still cached; heap essentially cold | 2.05 GB graph vs ~1.7 GB usable — cannot build, cannot cache |

Working answer, to be confirmed:

| | Build floor | Query floor |
|---|---|---|
| Meerkat | **2 GiB** | **2 GiB** |
| HNSW halfvec (stock tuning) | 8 GiB | 4 GiB |
| HNSW halfvec (`phase_pg_config` + serial build) | **4 GiB** | **4 GiB** |

Two things worth stating plainly about that table.

First, **`phase_pg_config` closes the build/query asymmetry for HNSW.** With
static tuning, HNSW needs a bigger box to build on than to serve from — the
classic reason a small deployment cannot self-host an HNSW index. Trading
`shared_buffers` for `maintenance_work_mem` for the duration of the build
removes that gap, and the honest comparison has to be made against the
lifted floor, not the stock one. Benchmarking HNSW at 8 GiB when 4 GiB
demonstrably works would be a straw man.

Second, even at the lifted floor the claim holds, just with a smaller
multiple: **HNSW needs 2x the RAM of Meerkat, and the remaining gap is a
hard floor rather than a tuning knob** — 2 GiB of RAM cannot hold a 2.05 GB
graph under any configuration, and no HNSW parameter makes that graph
smaller at 768d. The 2 GiB rung is where the two engines actually separate,
and it is the rung the ladder exists to measure.

## Running it

Every command below runs on your **workstation**. `rekall infra run` uploads
the configs, ensures the dataset is on the instance, and starts `rekall run`
there in a tmux session — so nothing in this section is typed over SSH. That
distinction matters: an earlier version of this runbook interleaved local
`infra` commands with bare `rekall load` / `index` / `search`, which only
work *on* the instance.

The build-once property comes from `rekall run` itself, not from splitting
phases: it skips any index that already exists (`lib.rs`, the
`exists && !rebuild` branch) and the load phase is resumable. So re-running
at a smaller rung reuses the 8 GiB build instead of rebuilding — which is
essential here, since rebuilding HNSW on the 4 GiB or 2 GiB box is exactly
what we are avoiding. Pass `--rebuild` only if you actually want a fresh
index.

```bash
# --- LOCAL: provision the 8 GiB box and build both engines' indexes -------
rekall infra up  -c infra/instances/m8g.large.toml

# Rung 1 (8 GiB). This run BUILDS the indexes and measures them; every later
# rung reuses them. Deliberately OUTSIDE the loop below: `infra resize` is
# not a no-op when the target type equals the current one -- it stops,
# modifies and starts the instance regardless -- so looping from the first
# rung would reboot the box between loading it and building on it, for
# nothing, and reassign its public IP on the way.
rekall infra run -c infra/instances/m8g.large.toml \
    benchmarks/cohere-1m-mkt-vs-hnsw.toml \
    --clean-run --comment "cohere-1m ladder: m8g.large 8 GiB"

rekall infra attach -c infra/instances/m8g.large.toml   # watch it; C-b d to detach

# --- LOCAL: walk down, search-only at each rung --------------------------
# init, load and index all skip at these rungs: the table has the right row
# count and the indexes already exist. Never pass --rebuild -- it would
# rebuild HNSW on the very boxes this experiment says cannot build it.
# --clean-run keeps each rung a separate published run instead of appending
# to the previous rung's results directory.
for rung in c8g.large c8g.medium; do
  rekall infra resize -c "infra/instances/$rung.toml"
  rekall infra run    -c "infra/instances/$rung.toml" \
      benchmarks/cohere-1m-mkt-vs-hnsw.toml \
      --clean-run --comment "cohere-1m ladder: $rung"
done

# --- LOCAL: apply the target to the published runs -----------------------
BASE=https://timescale.github.io/rekall/runs/cohere-1m
scripts/cost-frontier.py --recall 0.95 --qps 100 \
    m8g.large=$BASE/<date>/m8g.large-<hhmmss> \
    c8g.large=$BASE/<date>/c8g.large-<hhmmss> \
    c8g.medium=$BASE/<date>/c8g.medium-<hhmmss>

rekall infra down -c infra/instances/c8g.medium.toml
```

Find the published run paths in the catalog at
<https://timescale.github.io/rekall/>. `cost-frontier.py` reads a local
directory, a `summary.json` path, or a published run URL.

All four instance configs share `name_prefix = "rekall-1m-ladder"`, which is
what lets `-c <target>` resolve the same box after a resize re-tags it.

Both suites list the **same** arms, and must keep doing so: an arm's on-disk
index is named after the arm, so building one name and searching another
would simply find nothing.

Do 8 and 4 GiB first (~25 min each); run 2 GiB overnight, since HNSW walking
an uncached 2 GB graph can fall to single-digit QPS at high `ef_search`.

### If you want manual phase control

Only then do you need the instance shell — `rekall load` / `index` / `search`
are instance-side commands:

```bash
rekall infra ssh -c infra/instances/m8g.large.toml
# then, on the instance:
cd ~/rekall && rekall search benchmarks/cohere-1m-mkt-vs-hnsw.toml
```

`cohere-1m-build-floor.toml` is **not** part of this. It answers a separate
question — how small a box each engine can build *in place* on — and its
serial HNSW build produces a different graph, so its numbers are not
comparable with the ladder's. Reach for it only if the cost answer lands at
4 GiB or below and you want to confirm an in-place build gets there too.
