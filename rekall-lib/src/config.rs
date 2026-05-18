use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    Angular,
    Euclidean,
}

impl std::fmt::Display for Metric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Metric::Angular => write!(f, "angular"),
            Metric::Euclidean => write!(f, "euclidean"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetConfig {
    pub name: String,
    pub path: String,
    pub metric: Metric,
    pub dimension: u32,
}

impl DatasetConfig {
    pub fn resolved_path(&self) -> PathBuf {
        PathBuf::from(shellexpand::tilde(&self.path).as_ref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub dsn: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadConfig {
    pub table: Option<String>,
    #[serde(default = "default_parallelism")]
    pub parallelism: usize,
    #[serde(default = "default_chunk_size")]
    pub chunk_size: usize,
}

fn default_toml_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

fn default_parallelism() -> usize {
    1
}

fn default_chunk_size() -> usize {
    50_000
}

impl Default for LoadConfig {
    fn default() -> Self {
        Self {
            table: None,
            parallelism: default_parallelism(),
            chunk_size: default_chunk_size(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    #[serde(rename = "type")]
    pub index_type: String,
    pub name: String,
    #[serde(default = "default_toml_table")]
    pub params: toml::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default = "default_k")]
    pub k: usize,
    #[serde(default = "default_runs")]
    pub runs: usize,
    #[serde(default = "default_warmup")]
    pub warmup: usize,
    #[serde(default)]
    pub num_queries: Option<usize>,
    #[serde(default)]
    pub sweep: Vec<SweepConfig>,
}

fn default_k() -> usize {
    10
}

fn default_runs() -> usize {
    3
}

fn default_warmup() -> usize {
    100
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            k: default_k(),
            runs: default_runs(),
            warmup: default_warmup(),
            num_queries: None,
            sweep: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepConfig {
    pub index: String,
    #[serde(default)]
    pub params: HashMap<String, Vec<toml::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(default = "default_formats")]
    pub format: Vec<String>,
    pub json_path: Option<String>,
}

fn default_formats() -> Vec<String> {
    vec!["table".to_string()]
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            format: default_formats(),
            json_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    pub dataset: DatasetConfig,
    pub connection: ConnectionConfig,
    #[serde(default)]
    pub load: LoadConfig,
    #[serde(default)]
    pub index: Vec<IndexConfig>,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub output: OutputConfig,
}

impl BenchmarkConfig {
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            Error::Config(format!("Cannot read config file '{}': {e}", path.display()))
        })?;
        let config: Self = toml::from_str(&content).map_err(|e| {
            Error::Config(format!(
                "Invalid config file '{}': {e}",
                path.display()
            ))
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn table_name(&self) -> String {
        self.load
            .table
            .clone()
            .unwrap_or_else(|| self.dataset.name.replace('-', "_"))
    }

    fn validate(&self) -> Result<()> {
        for sweep in &self.search.sweep {
            if !self.index.iter().any(|idx| idx.name == sweep.index) {
                return Err(Error::SweepIndexNotFound {
                    name: sweep.index.clone(),
                });
            }
        }
        Ok(())
    }
}
