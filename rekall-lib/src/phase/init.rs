use crate::backend::postgres::PgConnection;
use crate::backend::DataLoader;
use crate::config::BenchmarkConfig;
use crate::error::Result;

pub async fn run(config: &BenchmarkConfig) -> Result<()> {
    let pg = PgConnection::new(
        &config.connection,
        &config.table_name(),
        config.dataset.dimension,
        config.dataset.metric,
    );

    pg.create_table().await?;
    pg.create_meta_tables().await?;

    Ok(())
}
