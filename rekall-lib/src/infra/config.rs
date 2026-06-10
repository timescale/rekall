use std::collections::HashMap;

use serde::{Deserialize, Serialize};

fn default_provider() -> String {
    "aws".to_string()
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_instance_type() -> String {
    "r7g.2xlarge".to_string()
}

fn default_disk_size() -> u32 {
    1000
}

fn default_pg_version() -> u32 {
    18
}

fn default_name_prefix() -> String {
    "rekall".to_string()
}

fn default_pg_port() -> u16 {
    5432
}

fn default_ubuntu_version() -> String {
    "26.04".to_string()
}

fn default_git_ref() -> String {
    "main".to_string()
}

/// Parse a PG memory-size string (e.g. "8GB", "256MB", "1024kB") into bytes.
/// PG accepts kB/MB/GB/TB suffixes, case-insensitive. Bare numbers are treated
/// as bytes per PG convention.
fn parse_size_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let upper = s.to_ascii_uppercase();
    let (num_part, mult): (&str, u64) = if let Some(n) = upper.strip_suffix("TB") {
        (&s[..n.len()], 1024u64.pow(4))
    } else if let Some(n) = upper.strip_suffix("GB") {
        (&s[..n.len()], 1024u64.pow(3))
    } else if let Some(n) = upper.strip_suffix("MB") {
        (&s[..n.len()], 1024u64.pow(2))
    } else if let Some(n) = upper.strip_suffix("KB") {
        (&s[..n.len()], 1024)
    } else {
        (s, 1)
    };
    num_part.trim().parse::<u64>().ok().map(|n| n * mult)
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfraConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default = "default_instance_type")]
    pub instance_type: String,
    #[serde(default = "default_disk_size")]
    pub disk_size_gb: u32,
    #[serde(default)]
    pub disk_iops: Option<u32>,
    #[serde(default)]
    pub disk_throughput: Option<u32>,
    #[serde(default = "default_pg_version")]
    pub pg_version: u32,
    #[serde(default = "default_pg_port")]
    pub pg_port: u16,
    /// Ubuntu LTS version for the AMI (e.g., "24.04", "26.04").
    /// Defaults to the latest LTS (26.04 "resolute"). Pinning an older
    /// version changes the kernel/glibc/compiler baseline and can shift
    /// benchmark numbers, so hold it fixed across a comparison.
    #[serde(default = "default_ubuntu_version")]
    pub ubuntu_version: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub pg_config: HashMap<String, String>,
    /// Enable huge pages: pre-allocates kernel huge-page pool sized to
    /// shared_buffers, disables THP, and sets PG `huge_pages = on`.
    /// Reduces TLB pressure for vector-index workloads on large
    /// shared_buffers but reserves the pool from normal RAM regardless
    /// of PG usage — so leave off if you want a fair A/B comparison.
    #[serde(default)]
    pub huge_pages: bool,
    /// Swap size in GB on NVMe instance store. Requires an instance
    /// type with local NVMe (e.g. r7gd, m7gd, i4i). The provisioning
    /// script auto-discovers the ephemeral device and creates a
    /// swapfile on it. Ignored if no instance store is found.
    /// vm.swappiness is set to 1 so swap only activates under memory
    /// pressure. Set to 0 or omit to disable.
    #[serde(default)]
    pub swap_gb: u32,
    #[serde(default)]
    pub rekall: Option<GitSourceConfig>,
    #[serde(default)]
    pub meerkat: Option<GitSourceConfig>,
    #[serde(default)]
    pub pgvector: Option<ExtVersionConfig>,
    #[serde(default)]
    pub vectorchord: Option<ExtVersionConfig>,
    /// Path to the user's SSH public key for instance access.
    /// Uploaded to AWS for the key pair. The corresponding private key
    /// (same path without .pub) is used locally for SSH connections.
    /// Required for `infra up`, but optional in config parsing so that
    /// the value can live in a gitignored .local.toml override.
    #[serde(default)]
    pub ssh_key: Option<String>,
    /// S3 bucket for datasets. Used to scope the IAM policy template.
    #[serde(default)]
    pub s3_bucket: Option<String>,
    /// Pre-created IAM instance profile name. If set, rekall uses this
    /// instead of creating IAM resources (which requires iam:CreateRole).
    /// Generate the template with `rekall infra iam-policy`.
    #[serde(default)]
    pub iam_instance_profile: Option<String>,
    #[serde(default = "default_name_prefix")]
    pub name_prefix: String,
}

impl Default for InfraConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            region: default_region(),
            instance_type: default_instance_type(),
            disk_size_gb: default_disk_size(),
            disk_iops: None,
            disk_throughput: None,
            pg_version: default_pg_version(),
            pg_port: default_pg_port(),
            ubuntu_version: default_ubuntu_version(),
            extensions: Vec::new(),
            pg_config: HashMap::new(),
            huge_pages: false,
            swap_gb: 0,
            rekall: None,
            meerkat: None,
            pgvector: None,
            vectorchord: None,
            ssh_key: None,
            s3_bucket: None,
            iam_instance_profile: None,
            name_prefix: default_name_prefix(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtVersionConfig {
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSourceConfig {
    pub git_url: String,
    #[serde(default = "default_git_ref")]
    pub git_ref: String,
}

impl InfraConfig {
    /// Load infra config from a standalone TOML file.
    /// Supports .local.toml overrides (same as benchmark configs).
    pub fn from_file(path: &std::path::Path) -> std::result::Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Cannot read '{}': {e}", path.display()))?;
        let mut base: toml::Value = toml::from_str(&content)
            .map_err(|e| format!("Invalid TOML '{}': {e}", path.display()))?;

        // Check for .local.toml override
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let parent = path.parent().unwrap_or(std::path::Path::new("."));
        let local_path = parent.join(format!("{stem}.local.toml"));
        if local_path.exists() {
            let local_content = std::fs::read_to_string(&local_path)
                .map_err(|e| format!("Cannot read '{}': {e}", local_path.display()))?;
            let local: toml::Value = toml::from_str(&local_content)
                .map_err(|e| format!("Invalid TOML '{}': {e}", local_path.display()))?;
            deep_merge(&mut base, local);
        }

        base.try_into()
            .map_err(|e| format!("Invalid infra config: {e}"))
    }

    pub fn pgvector_version(&self) -> &str {
        self.pgvector
            .as_ref()
            .and_then(|c| c.version.as_deref())
            .unwrap_or("v0.8.2")
    }

    pub fn vectorchord_version(&self) -> &str {
        self.vectorchord
            .as_ref()
            .and_then(|c| c.version.as_deref())
            .unwrap_or("1.1.1")
    }

    /// Ubuntu codename for the configured `ubuntu_version`. Used in the AMI
    /// name pattern (e.g. `ubuntu-noble-24.04-...`). Falls back to "noble"
    /// for unknown versions; `validate_ubuntu_version` catches those upfront.
    pub fn ubuntu_codename(&self) -> &'static str {
        match self.ubuntu_version.as_str() {
            "22.04" => "jammy",
            "24.04" => "noble",
            "26.04" => "resolute",
            _ => "noble",
        }
    }

    /// Reject `ubuntu_version` values we haven't tested. Adding a new LTS
    /// here also means trying the AMI search and full provision once.
    pub fn validate_ubuntu_version(&self) -> std::result::Result<(), String> {
        match self.ubuntu_version.as_str() {
            "22.04" | "24.04" | "26.04" => Ok(()),
            v => Err(format!(
                "Unsupported ubuntu_version '{v}'. Supported: 22.04 (jammy), \
                 24.04 (noble), 26.04 (resolute)"
            )),
        }
    }

    pub fn is_graviton(&self) -> bool {
        let ty = &self.instance_type;
        ty.contains("g.") || ty.contains("gd.") || ty.contains("gen.")
    }

    pub fn needs_github_access(&self) -> bool {
        let is_ssh_url = |url: &str| url.starts_with("git@") || url.starts_with("ssh://");
        if let Some(ref r) = self.rekall {
            if is_ssh_url(&r.git_url) {
                return true;
            }
        }
        if let Some(ref m) = self.meerkat {
            if is_ssh_url(&m.git_url) {
                return true;
            }
        }
        false
    }

    pub fn private_repo_names(&self) -> Vec<String> {
        let is_ssh_url = |url: &str| url.starts_with("git@") || url.starts_with("ssh://");
        let mut names = Vec::new();
        if let Some(ref r) = self.rekall {
            if is_ssh_url(&r.git_url) {
                names.push("rekall".to_string());
            }
        }
        if let Some(ref m) = self.meerkat {
            if is_ssh_url(&m.git_url) {
                names.push("meerkat".to_string());
            }
        }
        names
    }

    /// Default SSH public key path when `ssh_key` is not set in config.
    /// Matches the standard ed25519 location used by ssh and git.
    const DEFAULT_SSH_KEY: &'static str = "~/.ssh/id_ed25519.pub";

    pub fn ssh_public_key_path(&self) -> String {
        let raw = self.ssh_key.as_deref().unwrap_or(Self::DEFAULT_SSH_KEY);
        let expanded = shellexpand::tilde(raw).to_string();
        if expanded.ends_with(".pub") {
            expanded
        } else {
            format!("{expanded}.pub")
        }
    }

    pub fn ssh_private_key_path(&self) -> String {
        let raw = self.ssh_key.as_deref().unwrap_or(Self::DEFAULT_SSH_KEY);
        let expanded = shellexpand::tilde(raw).to_string();
        expanded.trim_end_matches(".pub").to_string()
    }

    pub fn require_ssh_key(&self) -> std::result::Result<(String, String), String> {
        Ok((self.ssh_public_key_path(), self.ssh_private_key_path()))
    }

    /// vCPU count parsed from the instance size suffix (e.g. `2xlarge` → 8).
    pub fn instance_vcpus(&self) -> Option<u32> {
        let size = self.instance_type.split('.').nth(1)?;
        Some(match size {
            "medium" => 1,
            "large" => 2,
            "xlarge" => 4,
            "2xlarge" => 8,
            "4xlarge" => 16,
            "8xlarge" => 32,
            "12xlarge" => 48,
            "16xlarge" => 64,
            "24xlarge" => 96,
            "metal" => 64,
            _ => return None,
        })
    }

    /// Estimate instance RAM in GB from instance type name.
    /// R-series: 8 GB/vCPU, M-series: 4 GB/vCPU, C-series: 2 GB/vCPU.
    pub fn instance_ram_gb(&self) -> Option<u32> {
        let family = self.instance_type.split('.').next()?;
        let gb_per_vcpu: u32 = if family.starts_with('r') || family.starts_with('x') {
            8
        } else if family.starts_with('m') {
            4
        } else if family.starts_with('c') {
            2
        } else {
            return None;
        };
        Some(gb_per_vcpu * self.instance_vcpus()?)
    }

    /// Build the effective pg_config by merging auto-computed defaults
    /// (from instance type) with explicit user overrides.
    pub fn effective_pg_config(&self) -> HashMap<String, String> {
        let mut config = HashMap::new();

        if let (Some(ram_gb), Some(vcpus)) = (self.instance_ram_gb(), self.instance_vcpus()) {
            let mut set = |k: &str, v: String| config.insert(k.to_string(), v);

            // Memory. shared_buffers = RAM/4 and effective_cache_size = 3·RAM/4
            // follow pgtune. maintenance_work_mem = RAM/2 is far above pgtune's
            // 8 GB cap, deliberately: it is a per-operation CEILING allocated
            // on demand (not reserved at startup, and never touched by
            // queries), so sizing it large simply lets a pgvector HNSW build
            // keep its whole graph resident in RAM instead of spilling to the
            // slow on-disk pass. Since we can't know the dataset in advance, we
            // err big — running out mid-build is far costlier than an unused
            // ceiling. RAM/4 (shared_buffers) + RAM/2 (a build that fills the
            // ceiling) = 3·RAM/4, leaving a RAM/4 cushion for the OS, (mostly
            // reclaimable) page cache, and workers; builds run one at a time
            // and pgvector shares the graph across parallel workers, not ×N.
            set("shared_buffers", format!("{}GB", ram_gb / 4));
            set("effective_cache_size", format!("{}GB", ram_gb * 3 / 4));
            set("maintenance_work_mem", format!("{}GB", (ram_gb / 2).max(1)));
            set("work_mem", "256MB".to_string());

            // WAL: fewer, larger checkpoints during bulk load + build.
            set("max_wal_size", "16GB".to_string());
            set("min_wal_size", "2GB".to_string());
            set("checkpoint_completion_target", "0.9".to_string());

            // Parallelism. pgvector's HNSW build uses
            // max_parallel_maintenance_workers DIRECTLY as its worker count.
            // Pin it to PHYSICAL cores — Graviton has no SMT (vCPU = cores),
            // Intel/AMD run 2 threads/core — because the build is CPU- and
            // lock-bound and SMT siblings mostly contend. Cap at 32: HNSW build
            // scaling is sublinear (workers share one graph under locking), so
            // returns flatten beyond that and extra workers just burn cores.
            // (This reproduces the hand-tuned r8i values — 4 on the 8-vCPU box,
            // 32 on the 96-vCPU box.) max_parallel_workers / max_worker_processes
            // must exceed it for the workers to actually launch.
            let physical_cores = if self.is_graviton() {
                vcpus.saturating_sub(1)
            } else {
                vcpus / 2
            };
            set(
                "max_parallel_maintenance_workers",
                physical_cores.clamp(1, 32).to_string(),
            );
            set("max_parallel_workers", vcpus.to_string());
            set("max_worker_processes", (vcpus * 2).to_string());
            set(
                "max_parallel_workers_per_gather",
                vcpus.div_ceil(2).to_string(),
            );

            // EBS gp3 / NVMe-class storage; JIT off for vector workloads (its
            // compile cost rarely pays off for ANN queries) — both per jkatz.
            set("effective_io_concurrency", "200".to_string());
            set("maintenance_io_concurrency", "200".to_string());
            set("random_page_cost", "1.1".to_string());
            set("jit", "off".to_string());
        }

        if self.huge_pages {
            config.insert("huge_pages".to_string(), "on".to_string());
        }

        for (k, v) in &self.pg_config {
            config.insert(k.clone(), v.clone());
        }

        config
    }

    /// Number of 2MB kernel huge pages to reserve, sized to shared_buffers
    /// with ~10% overhead for PG's other shared-memory allocations
    /// (lock tables, WAL buffers, etc.). Returns None if huge_pages is
    /// disabled or shared_buffers can't be parsed.
    pub fn nr_huge_pages(&self) -> Option<u64> {
        if !self.huge_pages {
            return None;
        }
        let pg = self.effective_pg_config();
        let bytes = parse_size_bytes(pg.get("shared_buffers")?)?;
        let pages = bytes / (2 * 1024 * 1024);
        Some(pages * 110 / 100)
    }

    pub fn to_tfvars(&self) -> serde_json::Value {
        let arch = if self.is_graviton() {
            "arm64"
        } else {
            "x86_64"
        };
        let mut vars = serde_json::json!({
            "region": self.region,
            "instance_type": self.instance_type,
            "arch": arch,
            "disk_size_gb": self.disk_size_gb,
            "pg_version": self.pg_version,
            "pg_port": self.pg_port,
            "ubuntu_version": self.ubuntu_version,
            "ubuntu_codename": self.ubuntu_codename(),
            "name_prefix": self.name_prefix,
        });

        if let Some(iops) = self.disk_iops {
            vars["disk_iops"] = serde_json::json!(iops);
        }
        if let Some(throughput) = self.disk_throughput {
            vars["disk_throughput"] = serde_json::json!(throughput);
        }
        if let Some(ref profile) = self.iam_instance_profile {
            vars["iam_instance_profile"] = serde_json::json!(profile);
        }
        if let Some(ref rekall) = self.rekall {
            vars["rekall_git_url"] = serde_json::json!(rekall.git_url);
            vars["rekall_git_ref"] = serde_json::json!(rekall.git_ref);
        }
        if let Some(ref meerkat) = self.meerkat {
            vars["meerkat_git_url"] = serde_json::json!(meerkat.git_url);
            vars["meerkat_git_ref"] = serde_json::json!(meerkat.git_ref);
        }

        vars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let cfg = InfraConfig::default();
        assert_eq!(cfg.provider, "aws");
        assert_eq!(cfg.region, "us-east-1");
        assert_eq!(cfg.instance_type, "r7g.2xlarge");
        assert_eq!(cfg.disk_size_gb, 1000);
        assert_eq!(cfg.pg_version, 18);
        assert_eq!(cfg.pg_port, 5432);
        assert!(cfg.ssh_key.is_none());
    }

    #[test]
    fn parse_minimal() {
        let toml_str = r#"
extensions = ["pgvector"]
"#;
        let cfg: InfraConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.extensions, vec!["pgvector"]);
        assert_eq!(cfg.region, "us-east-1");
        assert!(cfg.ssh_key.is_none());
    }

    #[test]
    fn parse_full() {
        let toml_str = r#"
provider = "aws"
region = "eu-west-1"
instance_type = "r6i.4xlarge"
disk_size_gb = 200
pg_version = 17
pg_port = 5433
extensions = ["pgvector", "meerkat"]
name_prefix = "bench"
ssh_key = "~/.ssh/bench_key"


[pg_config]
shared_buffers = "8GB"
maintenance_work_mem = "4GB"

[meerkat]
git_url = "git@github.com:tigerdata/meerkat.git"
git_ref = "feat/parallel-build"
"#;
        let cfg: InfraConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.region, "eu-west-1");
        assert_eq!(cfg.instance_type, "r6i.4xlarge");
        assert_eq!(cfg.disk_size_gb, 200);
        assert_eq!(cfg.pg_version, 17);
        assert_eq!(cfg.pg_port, 5433);
        assert_eq!(cfg.pg_config["shared_buffers"], "8GB");
        assert!(cfg.needs_github_access());
        let meerkat = cfg.meerkat.unwrap();
        assert_eq!(meerkat.git_ref, "feat/parallel-build");
    }

    #[test]
    fn needs_github_ssh() {
        let mut cfg = InfraConfig::default();
        assert!(!cfg.needs_github_access());

        cfg.meerkat = Some(GitSourceConfig {
            git_url: "https://github.com/pgvector/pgvector.git".to_string(),
            git_ref: "main".to_string(),
        });
        assert!(!cfg.needs_github_access());

        cfg.meerkat = Some(GitSourceConfig {
            git_url: "git@github.com:tigerdata/meerkat.git".to_string(),
            git_ref: "main".to_string(),
        });
        assert!(cfg.needs_github_access());
    }

    #[test]
    fn to_tfvars() {
        let cfg = InfraConfig {
            extensions: vec!["pgvector".to_string()],
            ..Default::default()
        };
        let vars = cfg.to_tfvars();
        assert_eq!(vars["region"], "us-east-1");
        // extensions, pg_config, huge_pages are now baked into the pre-rendered
        // user-data and no longer need to be tfvars.
        assert!(vars.get("extensions").is_none());
        assert!(vars.get("pg_config").is_none());
        assert!(vars.get("ssh_private_key_path").is_none());
    }

    #[test]
    fn auto_pg_config_r7g_2xlarge() {
        let cfg = InfraConfig {
            instance_type: "r7g.2xlarge".to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.instance_ram_gb(), Some(64));
        let pg = cfg.effective_pg_config();
        // 64 GB / 8 vCPU, Graviton (no SMT).
        assert_eq!(pg["shared_buffers"], "16GB"); // RAM/4
        assert_eq!(pg["maintenance_work_mem"], "32GB"); // RAM/2 (≫ pgtune cap)
        assert_eq!(pg["effective_cache_size"], "48GB"); // 3·RAM/4
        assert_eq!(pg["max_parallel_maintenance_workers"], "7"); // vCPU-1 (no SMT)
        assert_eq!(pg["max_parallel_workers"], "8");
        assert_eq!(pg["max_worker_processes"], "16");
        assert_eq!(pg["maintenance_io_concurrency"], "200");
        assert_eq!(pg["jit"], "off");
    }

    #[test]
    fn auto_pg_config_intel_uses_physical_cores_and_caps() {
        // Intel/AMD run 2 threads/core, so maintenance workers = vCPU/2,
        // capped at 32. These reproduce the hand-tuned r8i overrides.
        let small = InfraConfig {
            instance_type: "r8i.2xlarge".to_string(), // 8 vCPU / 64 GB
            ..Default::default()
        };
        let pg = small.effective_pg_config();
        assert_eq!(pg["max_parallel_maintenance_workers"], "4"); // 8/2
        assert_eq!(pg["maintenance_work_mem"], "32GB"); // RAM/2

        let big = InfraConfig {
            instance_type: "r8i.24xlarge".to_string(), // 96 vCPU / 768 GB
            ..Default::default()
        };
        let pg = big.effective_pg_config();
        assert_eq!(pg["max_parallel_maintenance_workers"], "32"); // 96/2 → cap 32
        assert_eq!(pg["maintenance_work_mem"], "384GB"); // RAM/2
    }

    #[test]
    fn ubuntu_version_default_is_latest_lts() {
        let cfg = InfraConfig::default();
        assert_eq!(cfg.ubuntu_version, "26.04");
        assert_eq!(cfg.ubuntu_codename(), "resolute");
        assert!(cfg.validate_ubuntu_version().is_ok());
    }

    #[test]
    fn ubuntu_version_26_04_maps_to_resolute() {
        let cfg = InfraConfig {
            ubuntu_version: "26.04".to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.ubuntu_codename(), "resolute");
        assert!(cfg.validate_ubuntu_version().is_ok());
        let vars = cfg.to_tfvars();
        assert_eq!(vars["ubuntu_version"], "26.04");
        assert_eq!(vars["ubuntu_codename"], "resolute");
    }

    #[test]
    fn ubuntu_version_unknown_is_rejected() {
        let cfg = InfraConfig {
            ubuntu_version: "24.10".to_string(),
            ..Default::default()
        };
        let err = cfg.validate_ubuntu_version().unwrap_err();
        assert!(err.contains("24.10"));
        assert!(err.contains("22.04"));
        assert!(err.contains("24.04"));
        assert!(err.contains("26.04"));
    }

    #[test]
    fn auto_pg_config_r8g_4xlarge() {
        let cfg = InfraConfig {
            instance_type: "r8g.4xlarge".to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.instance_ram_gb(), Some(128));
        let pg = cfg.effective_pg_config();
        // 128 GB / 16 vCPU, Graviton (no SMT).
        assert_eq!(pg["shared_buffers"], "32GB"); // RAM/4
        assert_eq!(pg["maintenance_work_mem"], "64GB"); // RAM/2
        assert_eq!(pg["effective_cache_size"], "96GB"); // 3·RAM/4
        assert_eq!(pg["max_parallel_maintenance_workers"], "15"); // vCPU-1 (no SMT)
        assert_eq!(pg["max_parallel_workers_per_gather"], "8"); // ceil(vCPU/2)
    }

    #[test]
    fn explicit_pg_config_overrides_auto() {
        let mut pg_config = HashMap::new();
        pg_config.insert("shared_buffers".to_string(), "8GB".to_string());
        pg_config.insert("huge_pages".to_string(), "try".to_string());

        let cfg = InfraConfig {
            instance_type: "r7g.2xlarge".to_string(),
            pg_config,
            ..Default::default()
        };
        let pg = cfg.effective_pg_config();
        assert_eq!(pg["shared_buffers"], "8GB"); // explicit override wins
        assert_eq!(pg["maintenance_work_mem"], "32GB"); // auto RAM/2
        assert_eq!(pg["huge_pages"], "try");
    }

    #[test]
    fn huge_pages_enabled_sets_pg_and_nr_pages() {
        let cfg = InfraConfig {
            instance_type: "r7g.2xlarge".to_string(),
            huge_pages: true,
            ..Default::default()
        };
        let pg = cfg.effective_pg_config();
        assert_eq!(pg["huge_pages"], "on");
        // 16GB shared_buffers / 2MB = 8192 pages, +10% overhead = 9011
        assert_eq!(cfg.nr_huge_pages(), Some(9011));
    }

    #[test]
    fn huge_pages_disabled_yields_none() {
        let cfg = InfraConfig {
            instance_type: "r7g.2xlarge".to_string(),
            ..Default::default()
        };
        let pg = cfg.effective_pg_config();
        assert!(!pg.contains_key("huge_pages"));
        assert_eq!(cfg.nr_huge_pages(), None);
    }

    #[test]
    fn huge_pages_user_override_wins() {
        let mut pg_config = HashMap::new();
        pg_config.insert("huge_pages".to_string(), "try".to_string());
        let cfg = InfraConfig {
            instance_type: "r7g.2xlarge".to_string(),
            huge_pages: true,
            pg_config,
            ..Default::default()
        };
        let pg = cfg.effective_pg_config();
        assert_eq!(pg["huge_pages"], "try");
        // Kernel pool still reserved — the flag drives the OS side regardless.
        // 16GB shared_buffers / 2MB = 8192 pages, +10% = 9011.
        assert_eq!(cfg.nr_huge_pages(), Some(9011));
    }

    #[test]
    fn parse_size_bytes_accepts_pg_suffixes() {
        assert_eq!(parse_size_bytes("8GB"), Some(8 * 1024u64.pow(3)));
        assert_eq!(parse_size_bytes("256MB"), Some(256 * 1024u64.pow(2)));
        assert_eq!(parse_size_bytes("1024kB"), Some(1024 * 1024));
        assert_eq!(parse_size_bytes("1024"), Some(1024));
        assert_eq!(parse_size_bytes("bogus"), None);
    }

    #[test]
    fn ssh_key_defaults_to_id_ed25519() {
        let cfg = InfraConfig::default();
        let (pub_path, priv_path) = cfg.require_ssh_key().unwrap();
        assert!(pub_path.ends_with("/.ssh/id_ed25519.pub"));
        assert!(priv_path.ends_with("/.ssh/id_ed25519"));
        assert!(!priv_path.ends_with(".pub"));
    }

    #[test]
    fn require_ssh_key_present() {
        let cfg = InfraConfig {
            ssh_key: Some("~/.ssh/mykey".to_string()),
            ..Default::default()
        };
        let (pub_path, priv_path) = cfg.require_ssh_key().unwrap();
        assert!(pub_path.contains("mykey.pub"));
        assert!(priv_path.contains("mykey"));
        assert!(!priv_path.contains(".pub"));
    }

    #[test]
    fn ssh_key_with_pub_extension() {
        let cfg = InfraConfig {
            ssh_key: Some("~/.ssh/mykey.pub".to_string()),
            ..Default::default()
        };
        let (pub_path, priv_path) = cfg.require_ssh_key().unwrap();
        assert!(pub_path.ends_with("mykey.pub"));
        assert!(priv_path.ends_with("mykey"));
    }
}
