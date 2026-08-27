use crate::backend::postgres::PgConnection;
use crate::backend::DataLoader;
use crate::config::BenchmarkConfig;
use crate::error::Result;

pub async fn run(config: &BenchmarkConfig) -> Result<()> {
    let mut pg = PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );
    // DDL and COPY both depend on the column's actual type.
    pg.set_storage(config.resolve_storage()?);

    pg.create_table().await?;
    pg.create_meta_tables().await?;

    Ok(())
}
