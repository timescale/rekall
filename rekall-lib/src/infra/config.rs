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
            extensions: Vec::new(),
            pg_config: HashMap::new(),
            huge_pages: false,
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

    pub fn ssh_public_key_path(&self) -> Option<String> {
        self.ssh_key.as_ref().map(|k| {
            let expanded = shellexpand::tilde(k).to_string();
            if expanded.ends_with(".pub") {
                expanded
            } else {
                format!("{expanded}.pub")
            }
        })
    }

    pub fn ssh_private_key_path(&self) -> Option<String> {
        self.ssh_key.as_ref().map(|k| {
            let expanded = shellexpand::tilde(k).to_string();
            expanded.trim_end_matches(".pub").to_string()
        })
    }

    pub fn require_ssh_key(&self) -> std::result::Result<(String, String), String> {
        let pub_path = self.ssh_public_key_path().ok_or_else(|| {
            "[infra].ssh_key is required. Set it in a .local.toml override, e.g.:\n\n\
             [infra]\n\
             ssh_key = \"~/.ssh/id_ed25519.pub\""
                .to_string()
        })?;
        let priv_path = self.ssh_private_key_path().unwrap();
        Ok((pub_path, priv_path))
    }

    /// Estimate instance RAM in GB from instance type name.
    /// R-series: 8 GB/vCPU, M-series: 4 GB/vCPU, C-series: 2 GB/vCPU.
    fn instance_ram_gb(&self) -> Option<u32> {
        let ty = &self.instance_type;
        let parts: Vec<&str> = ty.split('.').collect();
        if parts.len() != 2 {
            return None;
        }

        let family = parts[0];
        let size = parts[1];

        let gb_per_vcpu: u32 = if family.starts_with('r') || family.starts_with('x') {
            8
        } else if family.starts_with('m') {
            4
        } else if family.starts_with('c') {
            2
        } else {
            return None;
        };

        let vcpus: u32 = match size {
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
        };

        Some(gb_per_vcpu * vcpus)
    }

    /// Build the effective pg_config by merging auto-computed defaults
    /// (from instance type) with explicit user overrides.
    pub fn effective_pg_config(&self) -> HashMap<String, String> {
        let mut config = HashMap::new();

        if let Some(ram_gb) = self.instance_ram_gb() {
            let shared_buffers = ram_gb * 2 / 5;
            let maintenance_work_mem = ram_gb / 10;
            let effective_cache_size = ram_gb * 3 / 4;

            config.insert("shared_buffers".to_string(), format!("{shared_buffers}GB"));
            config.insert(
                "maintenance_work_mem".to_string(),
                format!("{maintenance_work_mem}GB"),
            );
            config.insert(
                "effective_cache_size".to_string(),
                format!("{effective_cache_size}GB"),
            );
            config.insert("work_mem".to_string(), "256MB".to_string());
            config.insert("max_wal_size".to_string(), "16GB".to_string());
            config.insert("min_wal_size".to_string(), "2GB".to_string());
            config.insert(
                "max_parallel_maintenance_workers".to_string(),
                "8".to_string(),
            );
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
        assert_eq!(pg["shared_buffers"], "25GB");
        assert_eq!(pg["maintenance_work_mem"], "6GB");
        assert_eq!(pg["effective_cache_size"], "48GB");
    }

    #[test]
    fn auto_pg_config_r8g_4xlarge() {
        let cfg = InfraConfig {
            instance_type: "r8g.4xlarge".to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.instance_ram_gb(), Some(128));
        let pg = cfg.effective_pg_config();
        assert_eq!(pg["shared_buffers"], "51GB");
        assert_eq!(pg["maintenance_work_mem"], "12GB");
        assert_eq!(pg["effective_cache_size"], "96GB");
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
        assert_eq!(pg["shared_buffers"], "8GB");
        assert_eq!(pg["maintenance_work_mem"], "6GB");
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
        // 25GB shared_buffers / 2MB = 12800 pages, +10% overhead = 14080
        assert_eq!(cfg.nr_huge_pages(), Some(14080));
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
        assert_eq!(cfg.nr_huge_pages(), Some(14080));
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
    fn require_ssh_key_missing() {
        let cfg = InfraConfig::default();
        assert!(cfg.require_ssh_key().is_err());
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
