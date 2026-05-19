use crate::backend::postgres::PgConnection;
use crate::backend::DataLoader;
use crate::config::BenchmarkConfig;
use crate::dataset::hdf5::Hdf5Reader;
use crate::dataset::DatasetReader;
use crate::error::{Error, Result};
use crate::progress::ProgressTx;

pub async fn run(config: &BenchmarkConfig, progress: &ProgressTx) -> Result<u64> {
    let pg = PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );

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
            .execute(&format!("TRUNCATE {} RESTART IDENTITY", pg.table_name), &[])
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
    Ok(loaded)
}
