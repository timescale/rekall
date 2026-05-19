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
        let config: Self = toml::from_str(&content)
            .map_err(|e| Error::Config(format!("Invalid config file '{}': {e}", path.display())))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    const MINIMAL_CONFIG: &str = r#"
[dataset]
name = "test-ds"
path = "/tmp/test.hdf5"
metric = "angular"
dimension = 128

[connection]
dsn = "host=localhost dbname=test"
"#;

    const FULL_CONFIG: &str = r#"
[dataset]
name = "cohere-1m"
path = "~/data/cohere.hdf5"
metric = "angular"
dimension = 768

[connection]
dsn = "host=localhost port=5434 dbname=postgres"

[load]
table = "my_table"
parallelism = 4
chunk_size = 10000

[[index]]
type = "meerkat"
name = "mkt-1k"
[index.params]
nlist = 1000

[[index]]
type = "pgvector-hnsw"
name = "hnsw-16"
[index.params]
m = 16

[search]
k = 10
runs = 3
warmup = 100

[[search.sweep]]
index = "mkt-1k"
[search.sweep.params]
"mkt.nprobe" = [5, 10, 20]

[[search.sweep]]
index = "hnsw-16"
[search.sweep.params]
"hnsw.ef_search" = [10, 40]

[output]
format = ["table", "json"]
json_path = "out.json"
"#;

    fn parse_config(toml_str: &str) -> BenchmarkConfig {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_str).unwrap();
        BenchmarkConfig::from_file(tmp.path()).unwrap()
    }

    #[test]
    fn parse_minimal() {
        let cfg = parse_config(MINIMAL_CONFIG);
        assert_eq!(cfg.dataset.name, "test-ds");
        assert_eq!(cfg.dataset.dimension, 128);
        assert_eq!(cfg.dataset.metric, Metric::Angular);
        assert_eq!(cfg.load.parallelism, 1);
        assert_eq!(cfg.load.chunk_size, 50_000);
        assert!(cfg.index.is_empty());
        assert!(cfg.search.sweep.is_empty());
    }

    #[test]
    fn parse_full() {
        let cfg = parse_config(FULL_CONFIG);
        assert_eq!(cfg.load.table, Some("my_table".to_string()));
        assert_eq!(cfg.load.parallelism, 4);
        assert_eq!(cfg.index.len(), 2);
        assert_eq!(cfg.index[0].index_type, "meerkat");
        assert_eq!(cfg.index[1].name, "hnsw-16");
        assert_eq!(cfg.search.k, 10);
        assert_eq!(cfg.search.sweep.len(), 2);
        assert_eq!(cfg.search.sweep[0].params["mkt.nprobe"].len(), 3);
        assert_eq!(cfg.output.format, vec!["table", "json"]);
        assert_eq!(cfg.output.json_path, Some("out.json".to_string()));
    }

    #[test]
    fn table_name_explicit() {
        let cfg = parse_config(FULL_CONFIG);
        assert_eq!(cfg.table_name(), "my_table");
    }

    #[test]
    fn table_name_derived() {
        let cfg = parse_config(MINIMAL_CONFIG);
        assert_eq!(cfg.table_name(), "test_ds");
    }

    #[test]
    fn resolved_path_tilde() {
        let cfg = parse_config(FULL_CONFIG);
        let path = cfg.dataset.resolved_path();
        assert!(!path.to_str().unwrap().contains('~'));
        assert!(path.to_str().unwrap().contains("data/cohere.hdf5"));
    }

    #[test]
    fn metric_display() {
        assert_eq!(Metric::Angular.to_string(), "angular");
        assert_eq!(Metric::Euclidean.to_string(), "euclidean");
    }

    #[test]
    fn sweep_validates_index_exists() {
        let toml_str = r#"
[dataset]
name = "test"
path = "/tmp/test.hdf5"
metric = "angular"
dimension = 128

[connection]
dsn = "host=localhost"

[[index]]
type = "meerkat"
name = "real-index"

[[search.sweep]]
index = "nonexistent"
[search.sweep.params]
"mkt.nprobe" = [5]
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml_str).unwrap();
        let err = BenchmarkConfig::from_file(tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nonexistent"), "error: {msg}");
    }

    #[test]
    fn defaults_applied() {
        let cfg = parse_config(MINIMAL_CONFIG);
        assert_eq!(cfg.search.k, 10);
        assert_eq!(cfg.search.runs, 3);
        assert_eq!(cfg.search.warmup, 100);
        assert_eq!(cfg.output.format, vec!["table"]);
    }

    #[test]
    fn missing_file_error() {
        let err = BenchmarkConfig::from_file(Path::new("/nonexistent.toml")).unwrap_err();
        assert!(err.to_string().contains("Cannot read config file"));
    }

    #[test]
    fn invalid_toml_error() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "this is not valid toml {{{").unwrap();
        let err = BenchmarkConfig::from_file(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("Invalid config file"));
    }
}
