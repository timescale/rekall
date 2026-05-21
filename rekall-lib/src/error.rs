use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Cannot connect to PostgreSQL at {dsn}: {source}")]
    PgConnect {
        dsn: String,
        source: tokio_postgres::Error,
    },

    #[error("{0}")]
    Pg(#[from] tokio_postgres::Error),

    #[error("Pool error: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),

    #[error("Dataset not found: {path}. Check [dataset].path in your config.")]
    DatasetNotFound { path: PathBuf },

    #[error("Invalid HDF5 dataset '{path}': {detail}")]
    Hdf5 { path: PathBuf, detail: String },

    #[error("Table '{table}' does not exist. Run `rekall init <config.toml>` first.")]
    TableNotFound { table: String },

    #[error("Table '{table}' has 0 rows. Run `rekall load <config.toml>` first.")]
    TableEmpty { table: String },

    #[error(
        "No index '{name}' found. \
         Run `rekall index <config.toml>` to build it."
    )]
    IndexNotFound { name: String },

    #[error(
        "Extension '{extension}' is not available. \
         Install {extension} before running this benchmark."
    )]
    ExtensionMissing { extension: String },

    #[error(
        "Dataset already fully loaded ({loaded}/{total} rows). \
         Use --force to reload."
    )]
    AlreadyLoaded { loaded: u64, total: u64 },

    #[error("Unknown backend type '{backend_type}'")]
    UnknownBackend { backend_type: String },

    #[error("Config error: {0}")]
    Config(String),

    #[error("Index '{name}' not found in config")]
    IndexConfigNotFound { name: String },

    #[error("Sweep references unknown index '{name}'")]
    SweepIndexNotFound { name: String },

    #[error(
        "Terraform not found. Install Terraform: \
         https://developer.hashicorp.com/terraform/install"
    )]
    TerraformNotFound,

    #[error("Terraform {command} failed: {detail}")]
    TerraformFailed { command: String, detail: String },

    #[error("No infrastructure running. Run `rekall infra up` first.")]
    InfraNotRunning,

    #[error(
        "AWS credentials not configured. Run `aws configure` or set \
         AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY environment variables."
    )]
    AwsCredentialsMissing,

    #[error("Infrastructure timeout: PostgreSQL not ready after {timeout_secs}s. SSH into the instance and check /var/log/rekall-setup.log")]
    InfraTimeout { timeout_secs: u64 },

    #[error("SSH key generation failed: {0}")]
    SshKeyGen(String),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
