// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::info;

use super::backend::InfraBackend;
use super::{InfraOutput, InfraStatus};
use crate::error::{Error, Result};

pub struct TerraformRunner {
    work_dir: PathBuf,
}

impl TerraformRunner {
    pub fn new(work_dir: PathBuf) -> Self {
        Self { work_dir }
    }

    pub fn check_installed() -> Result<String> {
        let output = Command::new("terraform")
            .arg("--version")
            .output()
            .map_err(|_| Error::TerraformNotFound)?;

        if !output.status.success() {
            return Err(Error::TerraformNotFound);
        }

        let version = String::from_utf8_lossy(&output.stdout);
        let first_line = version.lines().next().unwrap_or("unknown");
        Ok(first_line.to_string())
    }

    pub fn init(&self) -> Result<()> {
        let output = Command::new("terraform")
            .arg("init")
            .arg("-input=false")
            .current_dir(&self.work_dir)
            .output()
            .map_err(|e| Error::TerraformFailed {
                command: "init".to_string(),
                detail: e.to_string(),
            })?;

        if !output.status.success() {
            return Err(Error::TerraformFailed {
                command: "init".to_string(),
                detail: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    pub fn apply(&self) -> Result<()> {
        let status = Command::new("terraform")
            .args(["apply", "-auto-approve", "-input=false"])
            .current_dir(&self.work_dir)
            .status()
            .map_err(|e| Error::TerraformFailed {
                command: "apply".to_string(),
                detail: e.to_string(),
            })?;

        if !status.success() {
            return Err(Error::TerraformFailed {
                command: "apply".to_string(),
                detail: format!("exit code {}", status.code().unwrap_or(-1)),
            });
        }

        Ok(())
    }

    pub fn destroy(&self) -> Result<()> {
        let status = Command::new("terraform")
            .args(["destroy", "-auto-approve", "-input=false"])
            .current_dir(&self.work_dir)
            .status()
            .map_err(|e| Error::TerraformFailed {
                command: "destroy".to_string(),
                detail: e.to_string(),
            })?;

        if !status.success() {
            return Err(Error::TerraformFailed {
                command: "destroy".to_string(),
                detail: format!("exit code {}", status.code().unwrap_or(-1)),
            });
        }

        Ok(())
    }

    pub fn output(&self) -> Result<HashMap<String, String>> {
        let output = Command::new("terraform")
            .args(["output", "-json"])
            .current_dir(&self.work_dir)
            .output()
            .map_err(|e| Error::TerraformFailed {
                command: "output".to_string(),
                detail: e.to_string(),
            })?;

        if !output.status.success() {
            return Err(Error::TerraformFailed {
                command: "output".to_string(),
                detail: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let raw: HashMap<String, serde_json::Value> = serde_json::from_slice(&output.stdout)
            .map_err(|e| Error::TerraformFailed {
                command: "output".to_string(),
                detail: format!("cannot parse output JSON: {e}"),
            })?;

        let mut result = HashMap::new();
        for (key, val) in raw {
            if let Some(v) = val.get("value") {
                result.insert(key, v.as_str().unwrap_or(&v.to_string()).to_string());
            }
        }

        Ok(result)
    }

    pub fn state_exists(&self) -> bool {
        self.work_dir.join("terraform.tfstate").exists()
    }

    pub fn write_var_file(&self, vars: &serde_json::Value) -> Result<()> {
        let path = self.work_dir.join("terraform.tfvars.json");
        let content = serde_json::to_string_pretty(vars).map_err(|e| Error::TerraformFailed {
            command: "write_var_file".to_string(),
            detail: e.to_string(),
        })?;
        std::fs::write(&path, content).map_err(|e| Error::TerraformFailed {
            command: "write_var_file".to_string(),
            detail: format!("cannot write {}: {e}", path.display()),
        })?;
        Ok(())
    }

    fn parse_outputs(raw: &HashMap<String, String>) -> InfraOutput {
        InfraOutput {
            public_ip: raw.get("public_ip").cloned().unwrap_or_default(),
            instance_id: raw.get("instance_id").cloned().unwrap_or_default(),
            instance_type: raw.get("instance_type").cloned().unwrap_or_default(),
            dsn: raw.get("dsn").cloned().unwrap_or_default(),
            pg_port: raw.get("pg_port").cloned().unwrap_or_default(),
            iam_profile: raw.get("iam_instance_profile").cloned().unwrap_or_default(),
        }
    }

    fn parse_status(raw: &HashMap<String, String>) -> InfraStatus {
        let public_ip = raw.get("public_ip").cloned();
        InfraStatus {
            running: public_ip.is_some(),
            public_ip,
            instance_id: raw.get("instance_id").cloned(),
            instance_type: raw.get("instance_type").cloned(),
            instance_name: raw.get("instance_name").cloned(),
            dsn: raw.get("dsn").cloned(),
            iam_profile: raw.get("iam_instance_profile").cloned(),
            pg_ready: false,
        }
    }

    pub fn write_tf_files(dest: &Path, user_data: &str) -> Result<()> {
        let files = [
            (
                "main.tf",
                include_str!("../../../infra/terraform/aws/main.tf"),
            ),
            (
                "variables.tf",
                include_str!("../../../infra/terraform/aws/variables.tf"),
            ),
            (
                "outputs.tf",
                include_str!("../../../infra/terraform/aws/outputs.tf"),
            ),
            (
                "versions.tf",
                include_str!("../../../infra/terraform/aws/versions.tf"),
            ),
        ];

        std::fs::create_dir_all(dest).map_err(|e| Error::TerraformFailed {
            command: "write_tf_files".to_string(),
            detail: format!("cannot create {}: {e}", dest.display()),
        })?;

        for (name, content) in &files {
            let path = dest.join(name);
            std::fs::write(&path, content).map_err(|e| Error::TerraformFailed {
                command: "write_tf_files".to_string(),
                detail: format!("cannot write {}: {e}", path.display()),
            })?;
        }

        // user-data is pre-rendered by rekall (single source of truth shared
        // with the aws_native backend), then read by terraform via file().
        let user_data_path = dest.join("user-data.sh");
        std::fs::write(&user_data_path, user_data).map_err(|e| Error::TerraformFailed {
            command: "write_tf_files".to_string(),
            detail: format!("cannot write {}: {e}", user_data_path.display()),
        })?;

        Ok(())
    }
}

impl InfraBackend for TerraformRunner {
    fn name(&self) -> &str {
        "terraform"
    }

    fn up(
        &self,
        ssh_pub_key: &str,
        ssh_priv_path: &str,
        ssh_cidr: &str,
        deploy_key_paths: &[String],
        vars: &serde_json::Value,
        user_data: &str,
        _instance_name: Option<&str>,
    ) -> Result<InfraOutput> {
        let tf_version = Self::check_installed()?;
        info!("Terraform: {tf_version}");

        Self::write_tf_files(&self.work_dir, user_data)?;

        let mut all_vars = vars.clone();
        all_vars["ssh_public_key"] = serde_json::json!(ssh_pub_key);
        all_vars["ssh_private_key_path"] = serde_json::json!(ssh_priv_path);
        all_vars["ssh_cidr"] = serde_json::json!(ssh_cidr);
        if !deploy_key_paths.is_empty() {
            all_vars["github_ssh_key_paths"] = serde_json::json!(deploy_key_paths);
        }

        self.write_var_file(&all_vars)?;

        info!("Running terraform init");
        self.init()?;

        info!("Running terraform apply (includes provisioning)");
        self.apply()?;

        let raw = self.output()?;
        Ok(Self::parse_outputs(&raw))
    }

    fn down(&self) -> Result<()> {
        Self::check_installed()?;
        if !self.state_exists() {
            return Err(Error::InfraNotRunning);
        }
        info!("Running terraform destroy");
        self.destroy()?;
        info!("Infrastructure destroyed");
        Ok(())
    }

    fn status(&self) -> Result<InfraStatus> {
        if !self.state_exists() {
            return Ok(InfraStatus {
                running: false,
                public_ip: None,
                instance_id: None,
                instance_type: None,
                instance_name: None,
                dsn: None,
                iam_profile: None,
                pg_ready: false,
            });
        }
        let raw = self.output()?;
        Ok(Self::parse_status(&raw))
    }

    fn outputs(&self) -> Result<HashMap<String, String>> {
        self.output()
    }

    fn has_state(&self) -> bool {
        self.state_exists()
    }
}
