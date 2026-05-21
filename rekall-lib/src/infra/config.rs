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
    #[serde(default)]
    pub rekall: Option<GitSourceConfig>,
    #[serde(default)]
    pub meerkat: Option<GitSourceConfig>,
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
            rekall: None,
            meerkat: None,
            ssh_key: None,
            s3_bucket: None,
            iam_instance_profile: None,
            name_prefix: default_name_prefix(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSourceConfig {
    pub git_url: String,
    #[serde(default = "default_git_ref")]
    pub git_ref: String,
}

impl InfraConfig {
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
            let shared_buffers = ram_gb / 4;
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
            config.insert(
                "max_parallel_maintenance_workers".to_string(),
                "8".to_string(),
            );
        }

        for (k, v) in &self.pg_config {
            config.insert(k.clone(), v.clone());
        }

        config
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
            "extensions": self.extensions,
            "pg_config": self.effective_pg_config(),
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
        assert_eq!(vars["extensions"][0], "pgvector");
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
        assert_eq!(pg["shared_buffers"], "16GB");
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
        assert_eq!(pg["shared_buffers"], "32GB");
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
