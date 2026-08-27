use crate::backend::postgres::PgConnection;
use crate::backend::DataLoader;
use crate::config::BenchmarkConfig;
use crate::dataset::hdf5::Hdf5Reader;
use crate::dataset::DatasetReader;
use crate::error::{Error, Result};
use crate::progress::{ProgressEvent, ProgressTx};

/// Settle the freshly loaded heap so the first index build isn't special.
///
/// A bulk COPY leaves every tuple without hint bits. The first tuple-level
/// scan of the table — which is whichever index build happens to run first —
/// then clog-checks every row, dirties every heap page, and, with data
/// checksums on (the PG 18 initdb default), WAL-logs a full-page image per
/// page. On a multi-hundred-GB heap that is hundreds of GB of write I/O
/// charged to the first build's clock (measured: identical arms differed
/// 2-3x in build time depending on run order alone).
///
/// VACUUM (FREEZE, ANALYZE) pays that cost here, outside any timed region:
/// it sets hint bits, freezes tuples (no clog lookups for later scans
/// either), sets the visibility map, and emits the full-page-image WAL
/// once. The CHECKPOINT that follows flushes the dirty pages and WAL debt
/// so the first build starts from the same clean state as every later one.
/// CHECKPOINT needs superuser or the pg_checkpoint predefined role; if it
/// is denied the vacuum has still done the heavy lifting, so we warn and
/// continue.
async fn settle_after_load(pg: &PgConnection, progress: &ProgressTx) -> Result<()> {
    let client = pg.open_connection().await?;

    let settling = format!(
        "Settling '{}' after load: VACUUM (FREEZE, ANALYZE) so no \
         index build pays the first-scan hint-bit/WAL cost (this can \
         take a while on large tables)",
        pg.table_name,
    );
    tracing::info!("{settling}");
    let _ = progress.send(ProgressEvent::Message { text: settling });
    let start = std::time::Instant::now();
    client
        .execute(&format!("VACUUM (FREEZE, ANALYZE) {}", pg.table_name), &[])
        .await?;
    let vacuum_secs = start.elapsed().as_secs_f64();

    let start = std::time::Instant::now();
    let checkpoint = client.execute("CHECKPOINT", &[]).await;
    match &checkpoint {
        Ok(_) => {
            let settled = format!(
                "Settled '{}': vacuum {vacuum_secs:.1}s, checkpoint {:.1}s",
                pg.table_name,
                start.elapsed().as_secs_f64(),
            );
            tracing::info!("{settled}");
            let _ = progress.send(ProgressEvent::Message { text: settled });
        }
        Err(e) => {
            tracing::warn!(
                "CHECKPOINT after load failed ({e}); the load's dirty \
                 pages and WAL debt will be flushed during the first \
                 index build instead"
            );
            let settled = format!(
                "Settled '{}': vacuum {vacuum_secs:.1}s; CHECKPOINT \
                 denied — grant the pg_checkpoint role for fully \
                 order-independent builds",
                pg.table_name,
            );
            tracing::info!("{settled}");
            let _ = progress.send(ProgressEvent::Message { text: settled });
        }
    }
    Ok(())
}

pub async fn run(config: &BenchmarkConfig, progress: &ProgressTx) -> Result<u64> {
    let mut pg = PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );
    // DDL and COPY both depend on the column's actual type.
    pg.set_storage(config.resolve_storage()?);

    if !pg.table_exists().await? {
        return Err(Error::TableNotFound {
            table: config.table_name(),
        });
    }

    let existing = pg.row_count().await?;
    let dataset_path = config.dataset.resolved_path();
    let reader = Hdf5Reader::open(
        &dataset_path,
        config.dataset.metric,
        config.dataset.dimension,
    )?;
    let total = reader.meta().num_vectors;

    if existing >= total {
        return Err(Error::AlreadyLoaded {
            loaded: existing,
            total,
        });
    }

    if existing > 0 {
        tracing::warn!(
            "Table has {existing} rows but expected {total}. \
             Truncating and reloading."
        );
        let client = pg.open_connection().await?;
        client
            // Plain TRUNCATE: the table owns no sequences now that `id` is
            // a plain bigint, so RESTART IDENTITY would only have been a
            // no-op that implied otherwise.
            .execute(&format!("TRUNCATE {}", pg.table_name), &[])
            .await?;
    }

    // Channel to bridge blocking HDF5 reads -> async COPY writes.
    // Bounded to limit memory: at most 4 chunks buffered.
    let chunk_size = config.load.chunk_size;
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);

    let read_handle = tokio::task::spawn_blocking(move || {
        for chunk in reader.train_chunks(chunk_size) {
            if tx.blocking_send(chunk).is_err() {
                break;
            }
        }
    });

    let loaded = pg.load_data(total, &mut rx, progress).await?;

    let _ = read_handle.await;

    settle_after_load(&pg, progress).await?;

    Ok(loaded)
}
