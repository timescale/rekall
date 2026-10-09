// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

pub struct StateManager {
    base_dir: PathBuf,
}

/// A generated deploy key for a specific repo.
pub struct DeployKey {
    pub repo_name: String,
    pub public_key: String,
    pub private_key_path: PathBuf,
    pub generated: bool,
}

impl StateManager {
    pub fn new(name_prefix: &str) -> Result<Self> {
        let base = state_base_dir()?;
        Ok(Self {
            base_dir: base.join(name_prefix),
        })
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        let dirs = [deploy_key_dir()?, self.terraform_dir()];
        for dir in &dirs {
            std::fs::create_dir_all(dir).map_err(|e| {
                Error::Config(format!("Cannot create state dir '{}': {e}", dir.display()))
            })?;
        }
        Ok(())
    }

    pub fn terraform_dir(&self) -> PathBuf {
        self.base_dir.join("terraform")
    }

    pub fn has_deploy_key(&self, repo_name: &str) -> Result<bool> {
        Ok(repo_key_path(repo_name)?.exists() && repo_key_pub_path(repo_name)?.exists())
    }

    pub fn generate_deploy_key(&self, repo_name: &str) -> Result<DeployKey> {
        let key_path = repo_key_path(repo_name)?;
        let pub_path = repo_key_pub_path(repo_name)?;

        if key_path.exists() {
            std::fs::remove_file(&key_path).ok();
        }
        if pub_path.exists() {
            std::fs::remove_file(&pub_path).ok();
        }

        let output = Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-f",
                key_path.to_str().unwrap(),
                "-N",
                "",
                "-C",
                &format!("rekall/{repo_name}"),
            ])
            .output()
            .map_err(|e| Error::SshKeyGen(format!("ssh-keygen failed to start: {e}")))?;

        if !output.status.success() {
            return Err(Error::SshKeyGen(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).ok();
        }

        let public_key = std::fs::read_to_string(&pub_path).map_err(|e| {
            Error::SshKeyGen(format!(
                "Cannot read public key '{}': {e}",
                pub_path.display()
            ))
        })?;

        Ok(DeployKey {
            repo_name: repo_name.to_string(),
            public_key,
            private_key_path: key_path,
            generated: true,
        })
    }

    pub fn ensure_deploy_key(&self, repo_name: &str, regenerate: bool) -> Result<DeployKey> {
        if self.has_deploy_key(repo_name)? && !regenerate {
            let pub_path = repo_key_pub_path(repo_name)?;
            let public_key = std::fs::read_to_string(&pub_path).map_err(|e| {
                Error::SshKeyGen(format!(
                    "Cannot read public key '{}': {e}",
                    pub_path.display()
                ))
            })?;
            return Ok(DeployKey {
                repo_name: repo_name.to_string(),
                public_key,
                private_key_path: repo_key_path(repo_name)?,
                generated: false,
            });
        }
        self.generate_deploy_key(repo_name)
    }

    /// List all deploy key private key paths (for uploading to instances).
    pub fn deploy_key_paths(&self) -> Result<Vec<PathBuf>> {
        let dir = deploy_key_dir()?;
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut paths = Vec::new();
        let entries = std::fs::read_dir(&dir).map_err(|e| {
            Error::Config(format!(
                "Cannot read deploy key dir '{}': {e}",
                dir.display()
            ))
        })?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_none() {
                paths.push(path);
            }
        }

        paths.sort();
        Ok(paths)
    }
}

/// List all infra state directories (name_prefix values with state).
pub fn list_infra_names() -> Result<Vec<String>> {
    let base = state_base_dir()?;
    if !base.exists() {
        return Ok(Vec::new());
    }

    let mut names = Vec::new();
    let entries = std::fs::read_dir(&base)
        .map_err(|e| Error::Config(format!("Cannot read state dir '{}': {e}", base.display())))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name() {
                let name = name.to_string_lossy().to_string();
                if path.join("terraform").exists() {
                    names.push(name);
                }
            }
        }
    }

    names.sort();
    Ok(names)
}

pub fn detect_public_ip() -> Result<String> {
    let output = Command::new("curl")
        .args(["-s", "--max-time", "5", "https://checkip.amazonaws.com"])
        .output()
        .map_err(|e| Error::Config(format!("Cannot detect public IP: {e}")))?;

    if !output.status.success() {
        return Err(Error::Config(
            "Cannot detect public IP. Set ssh_cidr manually in [infra] config.".to_string(),
        ));
    }

    let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if ip.is_empty() || !ip.contains('.') {
        return Err(Error::Config(
            "Cannot detect public IP. Set ssh_cidr manually in [infra] config.".to_string(),
        ));
    }

    Ok(ip)
}

/// Deploy keys are account-wide, shared across every `name_prefix`. One
/// key per repo, registered once on GitHub, reused for every instance
/// you ever launch (regardless of instance type or suite name).
pub fn deploy_key_dir() -> Result<PathBuf> {
    Ok(state_base_dir()?.join("deploy-keys"))
}

fn repo_key_path(repo_name: &str) -> Result<PathBuf> {
    Ok(deploy_key_dir()?.join(repo_name))
}

fn repo_key_pub_path(repo_name: &str) -> Result<PathBuf> {
    Ok(deploy_key_dir()?.join(format!("{repo_name}.pub")))
}

fn state_base_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("REKALL_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }

    let cwd = std::env::current_dir()
        .map_err(|e| Error::Config(format!("Cannot determine current directory: {e}")))?;
    Ok(cwd.join(".rekall").join("infra"))
}
