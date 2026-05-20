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
    #[serde(default)]
    pub gucs: HashMap<String, toml::Value>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    #[default]
    Warm,
    Cold,
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
    pub cache: CacheMode,
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
            cache: CacheMode::default(),
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
    #[serde(default = "default_results_dir")]
    pub results_dir: String,
}

fn default_formats() -> Vec<String> {
    vec!["table".to_string()]
}

fn default_results_dir() -> String {
    "results".to_string()
}

impl OutputConfig {
    /// Get the output path for a specific index's results.
    /// Format: {results_dir}/{index_name}-{timestamp}.json
    pub fn index_results_path(&self, index_name: &str, timestamp: &str) -> String {
        format!("{}/{}-{}.json", self.results_dir, index_name, timestamp)
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            format: default_formats(),
            results_dir: default_results_dir(),
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
    /// Load config from a file or directory.
    ///
    /// **File mode**: loads the TOML file, merging any `.local.toml` sibling.
    ///
    /// **Directory mode**: loads `base.toml` from the directory for dataset,
    /// connection, and search defaults. Then loads every other `*.toml` file
    /// in the directory (sorted by name) and appends their `[[index]]` and
    /// `[[search.sweep]]` entries. Files named `*.local.toml` are also
    /// included (they're gitignored, not committed). This lets you organize
    /// config variants as separate files in a directory.
    pub fn from_path(path: &Path) -> Result<Self> {
        if path.is_dir() {
            Self::from_directory(path)
        } else {
            Self::from_file(path)
        }
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            Error::Config(format!("Cannot read config file '{}': {e}", path.display()))
        })?;
        let mut base: toml::Value = toml::from_str(&content)
            .map_err(|e| Error::Config(format!("Invalid config file '{}': {e}", path.display())))?;

        let local_path = local_config_path(path);
        if local_path.exists() {
            let local_content = std::fs::read_to_string(&local_path).map_err(|e| {
                Error::Config(format!(
                    "Cannot read local config '{}': {e}",
                    local_path.display()
                ))
            })?;
            let local: toml::Value = toml::from_str(&local_content).map_err(|e| {
                Error::Config(format!(
                    "Invalid local config '{}': {e}",
                    local_path.display()
                ))
            })?;
            deep_merge(&mut base, local);
            tracing::info!("Merged local overrides from {}", local_path.display());
        }

        let config: Self = base
            .try_into()
            .map_err(|e| Error::Config(format!("Invalid merged config: {e}")))?;
        config.validate()?;
        Ok(config)
    }

    fn from_directory(dir: &Path) -> Result<Self> {
        let base_path = dir.join("base.toml");
        if !base_path.exists() {
            return Err(Error::Config(format!(
                "Directory '{}' has no base.toml. \
                 Create base.toml with [dataset], [connection], and [search] sections.",
                dir.display()
            )));
        }

        let mut config = Self::from_file(&base_path)?;

        let mut index_files: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| Error::Config(format!("Cannot read directory '{}': {e}", dir.display())))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|e| e == "toml")
                    && p.file_name().unwrap_or_default().to_string_lossy() != "base.toml"
                    && !p
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .starts_with("base.")
            })
            .collect();
        index_files.sort();

        for index_file in &index_files {
            let content = std::fs::read_to_string(index_file).map_err(|e| {
                Error::Config(format!(
                    "Cannot read index config '{}': {e}",
                    index_file.display()
                ))
            })?;

            #[derive(Deserialize)]
            struct IndexFragment {
                #[serde(default)]
                index: Vec<IndexConfig>,
                #[serde(default)]
                search: Option<SearchFragment>,
            }

            #[derive(Deserialize)]
            struct SearchFragment {
                #[serde(default)]
                sweep: Vec<SweepConfig>,
            }

            let fragment: IndexFragment = toml::from_str(&content).map_err(|e| {
                Error::Config(format!(
                    "Invalid index config '{}': {e}",
                    index_file.display()
                ))
            })?;

            config.index.extend(fragment.index);
            if let Some(search) = fragment.search {
                config.search.sweep.extend(search.sweep);
            }

            tracing::info!("Loaded index config from {}", index_file.display());
        }

        config.validate()?;
        Ok(config)
    }

    /// Load base.toml from a directory, then only the specified index config files.
    pub fn from_directory_files(dir: &Path, files: &[PathBuf]) -> Result<Self> {
        let base_path = dir.join("base.toml");
        if !base_path.exists() {
            return Err(Error::Config(format!(
                "Directory '{}' has no base.toml. \
                 Create base.toml with [dataset], [connection], and [search] sections.",
                dir.display()
            )));
        }

        let mut config = Self::from_file(&base_path)?;

        let mut sorted_files = files.to_vec();
        sorted_files.sort();

        for index_file in &sorted_files {
            let content = std::fs::read_to_string(index_file).map_err(|e| {
                Error::Config(format!(
                    "Cannot read index config '{}': {e}",
                    index_file.display()
                ))
            })?;

            #[derive(Deserialize)]
            struct IndexFragment {
                #[serde(default)]
                index: Vec<IndexConfig>,
                #[serde(default)]
                search: Option<SearchFragment>,
            }

            #[derive(Deserialize)]
            struct SearchFragment {
                #[serde(default)]
                sweep: Vec<SweepConfig>,
            }

            let fragment: IndexFragment = toml::from_str(&content).map_err(|e| {
                Error::Config(format!(
                    "Invalid index config '{}': {e}",
                    index_file.display()
                ))
            })?;

            config.index.extend(fragment.index);
            if let Some(search) = fragment.search {
                config.search.sweep.extend(search.sweep);
            }

            tracing::info!("Loaded index config from {}", index_file.display());
        }

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

/// A named group of configs resolved from CLI paths.
#[derive(Debug)]
pub struct ConfigGroup {
    pub name: String,
    pub config: BenchmarkConfig,
}

/// Resolve a list of CLI paths into named config groups.
///
/// Rules:
/// - A directory becomes one group (all *.toml inside it)
/// - Individual files are grouped by parent directory
/// - A single file is its own group (named after the file stem)
pub fn resolve_config_groups(paths: &[PathBuf]) -> Result<Vec<ConfigGroup>> {
    if paths.is_empty() {
        return Err(Error::Config("No config paths provided".to_string()));
    }

    // Single path: simple case
    if paths.len() == 1 {
        let path = &paths[0];
        let name = path
            .file_stem()
            .or_else(|| path.file_name())
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let config = BenchmarkConfig::from_path(path)?;
        return Ok(vec![ConfigGroup { name, config }]);
    }

    // Multiple paths: group by parent directory
    let mut dir_groups: std::collections::BTreeMap<PathBuf, Vec<PathBuf>> =
        std::collections::BTreeMap::new();
    let mut standalone_dirs: Vec<PathBuf> = Vec::new();

    for path in paths {
        if path.is_dir() {
            standalone_dirs.push(path.clone());
        } else {
            let parent = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            dir_groups.entry(parent).or_default().push(path.clone());
        }
    }

    let mut groups = Vec::new();

    // Standalone directories: load all configs
    for dir in &standalone_dirs {
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let config = BenchmarkConfig::from_directory(dir)?;
        groups.push(ConfigGroup { name, config });
    }

    // File groups: load base.toml + only specified files
    for (dir, files) in &dir_groups {
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let config = BenchmarkConfig::from_directory_files(dir, files)?;
        groups.push(ConfigGroup { name, config });
    }

    Ok(groups)
}

/// Derive the local config path: `foo.toml` -> `foo.local.toml`
fn local_config_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let parent = path.parent().unwrap_or(Path::new("."));
    parent.join(format!("{stem}.local.toml"))
}

/// Recursively merge `overlay` into `base`. For tables, keys are merged
/// recursively. For all other types (scalars, arrays), the overlay value
/// replaces the base value entirely.
fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base.as_table_mut(), overlay) {
        (Some(base_table), toml::Value::Table(overlay_table)) => {
            for (key, overlay_val) in overlay_table {
                if let Some(base_val) = base_table.get_mut(&key) {
                    deep_merge(base_val, overlay_val);
                } else {
                    base_table.insert(key, overlay_val);
                }
            }
        }
        (_, overlay) => {
            *base = overlay;
        }
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

    #[test]
    fn deep_merge_scalars() {
        let mut base: toml::Value = toml::from_str("[a]\nx = 1\ny = 2").unwrap();
        let overlay: toml::Value = toml::from_str("[a]\ny = 99").unwrap();
        super::deep_merge(&mut base, overlay);
        assert_eq!(base["a"]["x"].as_integer(), Some(1));
        assert_eq!(base["a"]["y"].as_integer(), Some(99));
    }

    #[test]
    fn deep_merge_adds_new_keys() {
        let mut base: toml::Value = toml::from_str("[a]\nx = 1").unwrap();
        let overlay: toml::Value = toml::from_str("[a]\nz = 3").unwrap();
        super::deep_merge(&mut base, overlay);
        assert_eq!(base["a"]["x"].as_integer(), Some(1));
        assert_eq!(base["a"]["z"].as_integer(), Some(3));
    }

    #[test]
    fn deep_merge_array_replaces() {
        let mut base: toml::Value = toml::from_str("vals = [1, 2, 3]").unwrap();
        let overlay: toml::Value = toml::from_str("vals = [10, 20]").unwrap();
        super::deep_merge(&mut base, overlay);
        let arr = base["vals"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].as_integer(), Some(10));
    }

    #[test]
    fn deep_merge_nested_tables() {
        let mut base: toml::Value = toml::from_str("[a]\n[a.b]\nx = 1\ny = 2").unwrap();
        let overlay: toml::Value = toml::from_str("[a.b]\ny = 99\nz = 3").unwrap();
        super::deep_merge(&mut base, overlay);
        assert_eq!(base["a"]["b"]["x"].as_integer(), Some(1));
        assert_eq!(base["a"]["b"]["y"].as_integer(), Some(99));
        assert_eq!(base["a"]["b"]["z"].as_integer(), Some(3));
    }

    #[test]
    fn local_config_path_derivation() {
        assert_eq!(
            super::local_config_path(Path::new("examples/cohere-1m.toml")),
            PathBuf::from("examples/cohere-1m.local.toml")
        );
        assert_eq!(
            super::local_config_path(Path::new("/abs/path/bench.toml")),
            PathBuf::from("/abs/path/bench.local.toml")
        );
    }

    #[test]
    fn local_config_overrides_base() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("bench.toml");
        let local_path = dir.path().join("bench.local.toml");

        std::fs::write(&base_path, MINIMAL_CONFIG).unwrap();
        std::fs::write(
            &local_path,
            r#"
[search]
k = 50
runs = 1
"#,
        )
        .unwrap();

        let cfg = BenchmarkConfig::from_file(&base_path).unwrap();
        assert_eq!(cfg.search.k, 50);
        assert_eq!(cfg.search.runs, 1);
        assert_eq!(cfg.search.warmup, 100); // unchanged default
        assert_eq!(cfg.dataset.name, "test-ds"); // unchanged base
    }

    #[test]
    fn no_local_config_is_fine() {
        let cfg = parse_config(MINIMAL_CONFIG);
        assert_eq!(cfg.search.k, 10); // default, no local override
    }
}
