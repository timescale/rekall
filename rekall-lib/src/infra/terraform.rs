use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

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

    pub fn write_tf_files(dest: &Path) -> Result<()> {
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
            (
                "user-data.sh.tpl",
                include_str!("../../../infra/terraform/aws/user-data.sh.tpl"),
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

        Ok(())
    }
}
