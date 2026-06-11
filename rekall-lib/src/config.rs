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
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    pub metric: Metric,
    pub dimension: u32,
}

fn default_data_dir() -> String {
    dirs::data_dir()
        .map(|d| d.join("rekall/datasets").to_string_lossy().to_string())
        .unwrap_or_else(|| "data".to_string())
}

impl DatasetConfig {
    /// Resolve the local file path for this dataset.
    ///
    /// If `path` is set explicitly, use it (with tilde expansion).
    /// Otherwise derive from `data_dir` + the filename component of `uri`.
    pub fn resolved_path(&self) -> PathBuf {
        if let Some(ref path) = self.path {
            return PathBuf::from(shellexpand::tilde(path).as_ref());
        }
        if let Some(ref uri) = self.uri {
            let filename = uri.rsplit('/').next().unwrap_or("dataset.hdf5");
            let data_dir = shellexpand::tilde(&self.data_dir);
            return PathBuf::from(data_dir.as_ref()).join(filename);
        }
        PathBuf::from(shellexpand::tilde(&self.data_dir).as_ref()).join("dataset.hdf5")
    }

    /// Human-readable path string for display/dry-run output.
    pub fn display_path(&self) -> String {
        if let Some(ref path) = self.path {
            return path.clone();
        }
        self.resolved_path().display().to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    #[serde(default = "default_dsn")]
    pub dsn: String,
}

fn default_dsn() -> String {
    "host=localhost dbname=postgres".to_string()
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self { dsn: default_dsn() }
    }
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
    /// Table `parallel_workers` reloption to set for this index's build
    /// (reset afterwards). Meerkat's standard-AM build is derated by the
    /// planner's heap-size heuristic, so an explicit reloption lets it use
    /// the full `max_parallel_maintenance_workers`. Leave unset for pgvector
    /// HNSW, which reads `max_parallel_maintenance_workers` directly and
    /// would only be capped by a reloption.
    #[serde(default)]
    pub table_parallel_workers: Option<i32>,
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

/// How a `[[query]]` searches its target index.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryMode {
    /// Single-stage search against the index in its native distance — the
    /// ordinary `ORDER BY <col> <op> $1 LIMIT k`. For a binary-quantized
    /// (`bit`) index this is the "raw" Hamming query. Aliased as `raw`.
    #[default]
    #[serde(alias = "raw")]
    Default,
    /// Two-stage search: over-fetch a candidate pool from the index, then
    /// re-rank that pool by full-precision distance on the original `v`.
    /// The pool size is the swept `rerank_pool` param (the recall knob).
    Rerank,
}

/// Reserved sweep-param key carrying the rerank candidate-pool size.
/// Consumed by the backend (bound as a query `LIMIT`), not set as a GUC.
pub const RERANK_POOL_KEY: &str = "rerank_pool";

/// A named search to run against a built index. Many queries can target one
/// index (e.g. a raw and a rerank series over the same binary index), each
/// reported as its own result series under `name`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryConfig {
    /// Result-series label (the `index_name` in reports/plots).
    pub name: String,
    /// Name of the `[[index]]` this query searches.
    pub index: String,
    #[serde(default)]
    pub mode: QueryMode,
    /// Swept search params (GUCs, plus the reserved `rerank_pool`).
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
    /// Format: {results_dir}/{index_name}-{timestamp}.json.gz
    pub fn index_results_path(&self, index_name: &str, timestamp: &str) -> String {
        format!("{}/{}-{}.json.gz", self.results_dir, index_name, timestamp)
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

/// `[publish]` — archive results to S3 and publish reports to GitHub Pages.
/// All fields are optional; `enabled` gates auto-publish at the end of a run
/// (default OFF locally; `--publish` / `rekall infra run` turn it on).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishConfig {
    /// Auto-publish at the end of `rekall run` when true. Overridden by the
    /// `--publish` CLI flag; `rekall infra run` defaults it on for the remote.
    #[serde(default)]
    pub enabled: bool,
    /// S3 bucket for the raw-results archive. Defaults to `rekall` when unset.
    pub s3_bucket: Option<String>,
    /// AWS region for the bucket. Defaults to the credential chain's region,
    /// falling back to us-east-1.
    pub s3_region: Option<String>,
    /// Key prefix under the bucket. Runs land at `<prefix>/<run-id>/`.
    #[serde(default = "default_s3_prefix")]
    pub s3_prefix: String,
    /// Publish HTML reports + catalog to GitHub Pages (the `gh-pages` branch).
    #[serde(default = "default_true")]
    pub pages: bool,
    /// Local checkout of the rekall repo used to push `gh-pages`. Defaults to
    /// `~/repos/rekall` (the instance checkout from provisioning).
    pub repo_path: Option<String>,
    /// Branch that GitHub Pages serves from.
    #[serde(default = "default_pages_branch")]
    pub pages_branch: String,
    /// Optional public base URL (e.g. the github.io site) for catalog links.
    pub public_base_url: Option<String>,
}

fn default_s3_prefix() -> String {
    "runs".to_string()
}

fn default_true() -> bool {
    true
}

fn default_pages_branch() -> String {
    "gh-pages".to_string()
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            s3_bucket: None,
            s3_region: None,
            s3_prefix: default_s3_prefix(),
            pages: default_true(),
            repo_path: None,
            pages_branch: default_pages_branch(),
            public_base_url: None,
        }
    }
}

impl PublishConfig {
    /// Bucket name, defaulting to `rekall`.
    pub fn bucket(&self) -> &str {
        self.s3_bucket.as_deref().unwrap_or("rekall")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkConfig {
    pub dataset: DatasetConfig,
    #[serde(default)]
    pub connection: ConnectionConfig,
    #[serde(default)]
    pub load: LoadConfig,
    #[serde(default)]
    pub index: Vec<IndexConfig>,
    /// First-class named searches (many queries → one index). When empty,
    /// queries are synthesized from `[[search.sweep]]` for back-compat.
    #[serde(default)]
    pub query: Vec<QueryConfig>,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub publish: PublishConfig,
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
        let file_value: toml::Value = toml::from_str(&content)
            .map_err(|e| Error::Config(format!("Invalid config file '{}': {e}", path.display())))?;

        // Merge chain: global config → file → .local.toml
        let mut merged = load_global_config();
        deep_merge(&mut merged, file_value);

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
            deep_merge(&mut merged, local);
            tracing::info!("Merged local overrides from {}", local_path.display());
        }

        let config: Self = merged
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

        load_index_fragments(&mut config, &index_files)?;
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

        load_index_fragments(&mut config, &sorted_files)?;
        config.validate()?;
        Ok(config)
    }

    pub fn table_name(&self) -> String {
        self.load
            .table
            .clone()
            .unwrap_or_else(|| self.dataset.name.replace('-', "_"))
    }

    /// Load a dataset config and merge index fragments from separate paths.
    ///
    /// `dataset` is a TOML file with `[dataset]`, `[connection]`, etc.
    /// `index_paths` are individual index fragment files or directories
    /// containing index fragments (all `*.toml` files in the directory).
    pub fn from_dataset_and_indexes(dataset: &Path, index_paths: &[PathBuf]) -> Result<Self> {
        let mut config = Self::from_file(dataset)?;

        let expanded = expand_index_paths(index_paths)?;
        load_index_fragments(&mut config, &expanded)?;

        config.validate()?;
        Ok(config)
    }

    /// The searches to run: explicit `[[query]]` entries, plus a default
    /// (single-stage) query synthesized from each `[[search.sweep]]`. This
    /// keeps sweep-only configs working while letting new configs map many
    /// named queries onto one index.
    pub fn effective_queries(&self) -> Vec<QueryConfig> {
        let mut queries = self.query.clone();
        for sweep in &self.search.sweep {
            queries.push(QueryConfig {
                name: sweep.index.clone(),
                index: sweep.index.clone(),
                mode: QueryMode::Default,
                params: sweep.params.clone(),
            });
        }
        queries
    }

    fn validate(&self) -> Result<()> {
        for sweep in &self.search.sweep {
            if !self.index.iter().any(|idx| idx.name == sweep.index) {
                return Err(Error::SweepIndexNotFound {
                    name: sweep.index.clone(),
                });
            }
        }
        for query in &self.query {
            if !self.index.iter().any(|idx| idx.name == query.index) {
                return Err(Error::SweepIndexNotFound {
                    name: query.index.clone(),
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

/// A suite config that pairs datasets with shared index definitions.
///
/// ```toml
/// indexes = ["indexes/hnsw-m16.toml", "indexes/mkt-n4000.toml"]
///
/// [[suite]]
/// dataset = "datasets/cohere-1m.toml"
///
/// [[suite]]
/// dataset = "datasets/cohere-10m.toml"
/// indexes = ["indexes/hnsw-m16.toml"]  # override default
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct SuiteConfig {
    #[serde(default)]
    pub indexes: Vec<PathBuf>,
    pub suite: Vec<SuiteEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SuiteEntry {
    pub dataset: PathBuf,
    #[serde(default)]
    pub indexes: Option<Vec<PathBuf>>,
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

/// Resolve a suite config file into config groups.
///
/// Each `[[suite]]` entry produces one `ConfigGroup`. The dataset file
/// provides `[dataset]`, `[connection]`, `[search]`, etc. Index fragments
/// come from the entry's `indexes` (or the top-level default). Paths are
/// resolved relative to the suite file's parent directory.
pub fn resolve_suite(suite_path: &Path) -> Result<Vec<ConfigGroup>> {
    let content = std::fs::read_to_string(suite_path).map_err(|e| {
        Error::Config(format!(
            "Cannot read suite file '{}': {e}",
            suite_path.display()
        ))
    })?;
    let suite: SuiteConfig = toml::from_str(&content).map_err(|e| {
        Error::Config(format!(
            "Invalid suite file '{}': {e}",
            suite_path.display()
        ))
    })?;

    if suite.suite.is_empty() {
        return Err(Error::Config(format!(
            "Suite file '{}' has no [[suite]] entries",
            suite_path.display()
        )));
    }

    let base_dir = suite_path.parent().unwrap_or(Path::new("."));
    let mut groups = Vec::new();

    for entry in &suite.suite {
        let dataset_path = base_dir.join(&entry.dataset);
        let index_refs = entry.indexes.as_ref().unwrap_or(&suite.indexes);

        if index_refs.is_empty() {
            return Err(Error::Config(format!(
                "Suite entry for '{}' has no indexes (and no top-level default)",
                entry.dataset.display()
            )));
        }

        let abs_indexes: Vec<PathBuf> = index_refs.iter().map(|p| base_dir.join(p)).collect();
        let config = BenchmarkConfig::from_dataset_and_indexes(&dataset_path, &abs_indexes)?;
        let name = dataset_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        groups.push(ConfigGroup { name, config });
    }

    Ok(groups)
}

/// If `path` is a suite file (parses as SuiteConfig with at least one
/// `[[suite]]` entry), return the list of paths it references — every
/// index TOML and every dataset TOML, deduplicated, in the form they
/// appear in the suite file (relative to the suite's parent directory).
///
/// Returns `Ok(None)` for files that don't parse as a suite (regular
/// benchmark configs, or anything else). Used by `infra run` to figure
/// out what extra files to SCP alongside a suite TOML so the suite's
/// relative references resolve on the remote.
pub fn suite_dependencies(path: &Path) -> Result<Option<Vec<PathBuf>>> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            return Err(Error::Config(format!(
                "Cannot read '{}': {e}",
                path.display()
            )))
        }
    };
    let suite: SuiteConfig = match toml::from_str(&content) {
        Ok(s) => s,
        // Not a suite file — likely a regular benchmark config. Fine.
        Err(_) => return Ok(None),
    };
    if suite.suite.is_empty() {
        return Ok(None);
    }

    let mut deps: Vec<PathBuf> = Vec::new();
    deps.extend(suite.indexes.iter().cloned());
    for entry in &suite.suite {
        deps.push(entry.dataset.clone());
        if let Some(ref overrides) = entry.indexes {
            deps.extend(overrides.iter().cloned());
        }
    }
    deps.sort();
    deps.dedup();
    Ok(Some(deps))
}

/// Build config groups from explicit dataset and index paths (anonymous suite).
///
/// Each dataset is paired with all indexes, producing one `ConfigGroup` per dataset.
pub fn resolve_anonymous_suite(
    datasets: &[PathBuf],
    indexes: &[PathBuf],
) -> Result<Vec<ConfigGroup>> {
    if datasets.is_empty() {
        return Err(Error::Config(
            "--datasets requires at least one dataset file".to_string(),
        ));
    }
    if indexes.is_empty() {
        return Err(Error::Config(
            "--indexes requires at least one index file or directory".to_string(),
        ));
    }

    let mut groups = Vec::new();
    for dataset in datasets {
        let config = BenchmarkConfig::from_dataset_and_indexes(dataset, indexes)?;
        let name = dataset
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        groups.push(ConfigGroup { name, config });
    }
    Ok(groups)
}

/// Returns true if the file looks like a suite config (has `[[suite]]` entries).
pub fn is_suite_file(path: &Path) -> bool {
    if path.is_dir() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.contains("[[suite]]")
}

/// Expand index paths: directories become all their `*.toml` files, plain
/// files pass through. Results are sorted for deterministic order.
fn expand_index_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut expanded = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut dir_files: Vec<PathBuf> = std::fs::read_dir(path)
                .map_err(|e| {
                    Error::Config(format!(
                        "Cannot read index directory '{}': {e}",
                        path.display()
                    ))
                })?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().is_some_and(|e| e == "toml")
                        && !p
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .contains(".local.")
                })
                .collect();
            dir_files.sort();
            expanded.extend(dir_files);
        } else {
            expanded.push(path.clone());
        }
    }
    Ok(expanded)
}

/// Load index fragments from files into an existing config.
fn load_index_fragments(config: &mut BenchmarkConfig, files: &[PathBuf]) -> Result<()> {
    #[derive(Deserialize)]
    struct IndexFragment {
        #[serde(default)]
        index: Vec<IndexConfig>,
        #[serde(default)]
        query: Vec<QueryConfig>,
        #[serde(default)]
        search: Option<SearchFragment>,
    }

    #[derive(Deserialize)]
    struct SearchFragment {
        #[serde(default)]
        sweep: Vec<SweepConfig>,
    }

    for index_file in files {
        let content = std::fs::read_to_string(index_file).map_err(|e| {
            Error::Config(format!(
                "Cannot read index config '{}': {e}",
                index_file.display()
            ))
        })?;

        let fragment: IndexFragment = toml::from_str(&content).map_err(|e| {
            Error::Config(format!(
                "Invalid index config '{}': {e}",
                index_file.display()
            ))
        })?;

        config.index.extend(fragment.index);
        config.query.extend(fragment.query);
        if let Some(search) = fragment.search {
            config.search.sweep.extend(search.sweep);
        }

        tracing::info!("Loaded index config from {}", index_file.display());
    }

    Ok(())
}

/// Path to the global rekall config file.
pub fn global_config_path() -> PathBuf {
    dirs::config_dir()
        .map(|d| d.join("rekall/config.toml"))
        .unwrap_or_else(|| PathBuf::from("~/.config/rekall/config.toml"))
}

/// Load the global config as a raw TOML value. Returns an empty table
/// if the file doesn't exist. Logs a warning on parse errors rather
/// than failing — a broken global config shouldn't block all runs.
fn load_global_config() -> toml::Value {
    let path = global_config_path();
    if !path.exists() {
        return toml::Value::Table(toml::map::Map::new());
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => match toml::from_str(&content) {
            Ok(value) => {
                tracing::debug!("Loaded global config from {}", path.display());
                value
            }
            Err(e) => {
                tracing::warn!("Invalid global config '{}': {e}", path.display());
                toml::Value::Table(toml::map::Map::new())
            }
        },
        Err(e) => {
            tracing::warn!("Cannot read global config '{}': {e}", path.display());
            toml::Value::Table(toml::map::Map::new())
        }
    }
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

    const QUERY_CONFIG: &str = r#"
[dataset]
name = "test-ds"
path = "data/test.hdf5"
dimension = 768
metric = "angular"

[[index]]
type = "pgvector-hnsw"
name = "hnsw-bq"
[index.params]
column_type = "bit"

[[query]]
name = "bq-raw"
index = "hnsw-bq"
mode = "raw"
[query.params]
"hnsw.ef_search" = [40, 80]

[[query]]
name = "bq-rerank"
index = "hnsw-bq"
mode = "rerank"
[query.params]
"hnsw.ef_search" = [40, 80]
rerank_pool = [100, 200]
"#;

    #[test]
    fn parse_first_class_queries() {
        let cfg = parse_config(QUERY_CONFIG);
        assert_eq!(cfg.query.len(), 2);
        assert_eq!(cfg.query[0].name, "bq-raw");
        assert_eq!(cfg.query[0].index, "hnsw-bq");
        // `raw` is an accepted alias for the default single-stage mode.
        assert_eq!(cfg.query[0].mode, QueryMode::Default);
        assert_eq!(cfg.query[1].mode, QueryMode::Rerank);
        assert_eq!(cfg.query[1].params[RERANK_POOL_KEY].len(), 2);
    }

    #[test]
    fn query_mode_defaults_to_default() {
        let q: QueryConfig = toml::from_str("name = \"q\"\nindex = \"i\"").unwrap();
        assert_eq!(q.mode, QueryMode::Default);
    }

    #[test]
    fn effective_queries_uses_explicit_queries() {
        let cfg = parse_config(QUERY_CONFIG);
        let queries = cfg.effective_queries();
        // No sweeps → exactly the two explicit queries.
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].name, "bq-raw");
    }

    #[test]
    fn effective_queries_synthesizes_from_sweeps() {
        let cfg = parse_config(FULL_CONFIG);
        // FULL_CONFIG has 2 sweeps and no [[query]] entries.
        assert!(cfg.query.is_empty());
        let queries = cfg.effective_queries();
        assert_eq!(queries.len(), 2);
        // Synthesized queries are named after the index, single-stage.
        assert_eq!(queries[0].name, queries[0].index);
        assert_eq!(queries[0].mode, QueryMode::Default);
    }

    #[test]
    fn validate_rejects_query_with_unknown_index() {
        let bad = r#"
[dataset]
name = "test-ds"
path = "data/test.hdf5"
dimension = 768
metric = "angular"

[[query]]
name = "q"
index = "does-not-exist"
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bad).unwrap();
        let err = BenchmarkConfig::from_file(tmp.path());
        assert!(matches!(err, Err(Error::SweepIndexNotFound { .. })));
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
    fn resolved_path_from_uri_and_data_dir() {
        let toml_str = r#"
[dataset]
name = "test"
uri = "s3://bucket/path/to/data.hdf5"
data_dir = "my-data"
metric = "angular"
dimension = 128

[connection]
dsn = "host=localhost"
"#;
        let cfg = parse_config(toml_str);
        assert_eq!(
            cfg.dataset.resolved_path(),
            PathBuf::from("my-data/data.hdf5")
        );
        assert!(cfg.dataset.display_path().contains("my-data/data.hdf5"));
    }

    #[test]
    fn resolved_path_explicit_overrides_uri() {
        let toml_str = r#"
[dataset]
name = "test"
path = "/explicit/path.hdf5"
uri = "s3://bucket/different.hdf5"
data_dir = "ignored"
metric = "angular"
dimension = 128

[connection]
dsn = "host=localhost"
"#;
        let cfg = parse_config(toml_str);
        assert_eq!(
            cfg.dataset.resolved_path(),
            PathBuf::from("/explicit/path.hdf5")
        );
    }

    #[test]
    fn resolved_path_default_data_dir() {
        let toml_str = r#"
[dataset]
name = "test"
uri = "https://example.com/vectors.hdf5"
metric = "angular"
dimension = 128

[connection]
dsn = "host=localhost"
"#;
        let cfg = parse_config(toml_str);
        let path = cfg.dataset.resolved_path();
        assert!(
            path.ends_with("vectors.hdf5"),
            "expected path ending in vectors.hdf5, got: {}",
            path.display()
        );
        // Default data_dir uses XDG data dir (~/.local/share/rekall/datasets)
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("rekall") || path_str.contains("data"),
            "expected rekall or data in path, got: {path_str}"
        );
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
            super::local_config_path(Path::new("benchmarks/datasets/cohere-1m.toml")),
            PathBuf::from("benchmarks/datasets/cohere-1m.local.toml")
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

    const INDEX_HNSW: &str = r#"
[[index]]
type = "pgvector-hnsw"
name = "hnsw-m16"
[index.params]
m = 16

[[search.sweep]]
index = "hnsw-m16"
[search.sweep.params]
"hnsw.ef_search" = [10, 40]
"#;

    const INDEX_MEERKAT: &str = r#"
[[index]]
type = "meerkat"
name = "mkt-1k"
[index.params]
nlist = 1000

[[search.sweep]]
index = "mkt-1k"
[search.sweep.params]
"mkt.nprobe" = [5, 10]
"#;

    fn write_suite_dir(dir: &Path) {
        let datasets = dir.join("datasets");
        let indexes = dir.join("indexes");
        std::fs::create_dir_all(&datasets).unwrap();
        std::fs::create_dir_all(&indexes).unwrap();
        std::fs::write(datasets.join("ds-a.toml"), MINIMAL_CONFIG).unwrap();
        std::fs::write(
            datasets.join("ds-b.toml"),
            MINIMAL_CONFIG.replace("test-ds", "test-ds-b"),
        )
        .unwrap();
        std::fs::write(indexes.join("hnsw.toml"), INDEX_HNSW).unwrap();
        std::fs::write(indexes.join("mkt.toml"), INDEX_MEERKAT).unwrap();
    }

    #[test]
    fn suite_file_loads_all_entries() {
        let dir = tempfile::tempdir().unwrap();
        write_suite_dir(dir.path());
        let suite_path = dir.path().join("suite.toml");
        std::fs::write(
            &suite_path,
            r#"
indexes = ["indexes/hnsw.toml", "indexes/mkt.toml"]

[[suite]]
dataset = "datasets/ds-a.toml"

[[suite]]
dataset = "datasets/ds-b.toml"
"#,
        )
        .unwrap();

        let groups = resolve_suite(&suite_path).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "ds-a");
        assert_eq!(groups[0].config.dataset.name, "test-ds");
        assert_eq!(groups[0].config.index.len(), 2);
        assert_eq!(groups[1].name, "ds-b");
        assert_eq!(groups[1].config.dataset.name, "test-ds-b");
        assert_eq!(groups[1].config.index.len(), 2);
    }

    #[test]
    fn suite_entry_overrides_indexes() {
        let dir = tempfile::tempdir().unwrap();
        write_suite_dir(dir.path());
        let suite_path = dir.path().join("suite.toml");
        std::fs::write(
            &suite_path,
            r#"
indexes = ["indexes/hnsw.toml", "indexes/mkt.toml"]

[[suite]]
dataset = "datasets/ds-a.toml"

[[suite]]
dataset = "datasets/ds-b.toml"
indexes = ["indexes/hnsw.toml"]
"#,
        )
        .unwrap();

        let groups = resolve_suite(&suite_path).unwrap();
        assert_eq!(groups[0].config.index.len(), 2);
        assert_eq!(groups[1].config.index.len(), 1);
        assert_eq!(groups[1].config.index[0].name, "hnsw-m16");
    }

    #[test]
    fn suite_dependencies_extracts_referenced_paths() {
        let dir = tempfile::tempdir().unwrap();
        write_suite_dir(dir.path());
        let suite_path = dir.path().join("suite.toml");
        std::fs::write(
            &suite_path,
            r#"
indexes = ["indexes/hnsw.toml", "indexes/mkt.toml"]

[[suite]]
dataset = "datasets/ds-a.toml"

[[suite]]
dataset = "datasets/ds-b.toml"
indexes = ["indexes/hnsw.toml"]
"#,
        )
        .unwrap();

        let deps = suite_dependencies(&suite_path).unwrap().unwrap();
        // Deduplicated + sorted; the per-entry override doesn't add a new entry
        // because hnsw.toml is already in the top-level indexes list.
        assert_eq!(
            deps,
            vec![
                PathBuf::from("datasets/ds-a.toml"),
                PathBuf::from("datasets/ds-b.toml"),
                PathBuf::from("indexes/hnsw.toml"),
                PathBuf::from("indexes/mkt.toml"),
            ]
        );
    }

    #[test]
    fn suite_dependencies_returns_none_for_non_suite_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bench.toml");
        std::fs::write(
            &path,
            r#"
[dataset]
name = "foo"
path = "data/foo.hdf5"
metric = "angular"
dimension = 768

[connection]
dsn = "host=localhost"
"#,
        )
        .unwrap();
        // Regular benchmark config — has no [[suite]] entries.
        assert!(suite_dependencies(&path).unwrap().is_none());
    }

    #[test]
    fn suite_with_index_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_suite_dir(dir.path());
        let suite_path = dir.path().join("suite.toml");
        std::fs::write(
            &suite_path,
            r#"
indexes = ["indexes/"]

[[suite]]
dataset = "datasets/ds-a.toml"
"#,
        )
        .unwrap();

        let groups = resolve_suite(&suite_path).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].config.index.len(), 2);
    }

    #[test]
    fn suite_empty_entries_error() {
        let dir = tempfile::tempdir().unwrap();
        let suite_path = dir.path().join("suite.toml");
        std::fs::write(
            &suite_path,
            r#"
indexes = ["indexes/hnsw.toml"]
suite = []
"#,
        )
        .unwrap();

        let err = resolve_suite(&suite_path).unwrap_err();
        assert!(err.to_string().contains("no [[suite]] entries"));
    }

    #[test]
    fn suite_no_indexes_error() {
        let dir = tempfile::tempdir().unwrap();
        write_suite_dir(dir.path());
        let suite_path = dir.path().join("suite.toml");
        std::fs::write(
            &suite_path,
            r#"
[[suite]]
dataset = "datasets/ds-a.toml"
"#,
        )
        .unwrap();

        let err = resolve_suite(&suite_path).unwrap_err();
        assert!(err.to_string().contains("no indexes"));
    }

    #[test]
    fn anonymous_suite() {
        let dir = tempfile::tempdir().unwrap();
        write_suite_dir(dir.path());

        let datasets = vec![
            dir.path().join("datasets/ds-a.toml"),
            dir.path().join("datasets/ds-b.toml"),
        ];
        let indexes = vec![dir.path().join("indexes/hnsw.toml")];

        let groups = resolve_anonymous_suite(&datasets, &indexes).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].config.index.len(), 1);
        assert_eq!(groups[0].config.index[0].name, "hnsw-m16");
        assert_eq!(groups[1].config.index.len(), 1);
    }

    #[test]
    fn anonymous_suite_with_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_suite_dir(dir.path());

        let datasets = vec![dir.path().join("datasets/ds-a.toml")];
        let indexes = vec![dir.path().join("indexes")];

        let groups = resolve_anonymous_suite(&datasets, &indexes).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].config.index.len(), 2);
    }

    #[test]
    fn dataset_and_indexes_method() {
        let dir = tempfile::tempdir().unwrap();
        write_suite_dir(dir.path());

        let cfg = BenchmarkConfig::from_dataset_and_indexes(
            &dir.path().join("datasets/ds-a.toml"),
            &[
                dir.path().join("indexes/hnsw.toml"),
                dir.path().join("indexes/mkt.toml"),
            ],
        )
        .unwrap();

        assert_eq!(cfg.dataset.name, "test-ds");
        assert_eq!(cfg.index.len(), 2);
        assert_eq!(cfg.search.sweep.len(), 2);
    }

    #[test]
    fn is_suite_file_detection() {
        let dir = tempfile::tempdir().unwrap();

        let suite = dir.path().join("suite.toml");
        std::fs::write(&suite, "[[suite]]\ndataset = \"foo.toml\"").unwrap();
        assert!(is_suite_file(&suite));

        let regular = dir.path().join("regular.toml");
        std::fs::write(&regular, MINIMAL_CONFIG).unwrap();
        assert!(!is_suite_file(&regular));

        assert!(!is_suite_file(dir.path()));
    }
}
