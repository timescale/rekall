// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

mod aws_native;
mod backend;
pub mod config;
pub(crate) mod state;
mod terraform;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use tracing::info;

use aws_native::AwsNativeBackend;
use backend::InfraBackend;
use config::InfraConfig;
use state::StateManager;
use terraform::TerraformRunner;

use crate::error::{Error, Result};

#[derive(Debug)]
pub struct InfraOutput {
    pub public_ip: String,
    pub instance_id: String,
    pub instance_type: String,
    pub dsn: String,
    pub pg_port: String,
    pub iam_profile: String,
}

#[derive(Debug)]
pub struct InfraStatus {
    pub running: bool,
    pub public_ip: Option<String>,
    pub instance_id: Option<String>,
    pub instance_type: Option<String>,
    /// AWS `Name` tag — unique per instance (timestamp-suffix). For
    /// distinguishing multiple instances under the same name_prefix.
    pub instance_name: Option<String>,
    pub dsn: Option<String>,
    pub iam_profile: Option<String>,
    pub pg_ready: bool,
}

pub struct InfraManager {
    config: InfraConfig,
    state: StateManager,
    backend: Box<dyn InfraBackend>,
}

impl InfraManager {
    pub fn new(config: &InfraConfig) -> Result<Self> {
        let state = StateManager::new(&config.name_prefix)?;
        let backend: Box<dyn InfraBackend> = match config.provider.as_str() {
            "aws" => Box::new(AwsNativeBackend::new(
                state.base_dir().to_path_buf(),
                config,
            )),
            "terraform" => Box::new(TerraformRunner::new(state.terraform_dir())),
            other => {
                return Err(Error::Config(format!(
                    "Unknown infra provider '{other}'. Supported: aws, terraform"
                )));
            }
        };
        Ok(Self {
            config: config.clone(),
            state,
            backend,
        })
    }

    pub fn up(&self) -> Result<InfraOutput> {
        self.up_with_name(None)
    }

    /// Variant of `up()` that lets the caller override the AWS Name tag
    /// for the new instance. Passing `None` uses the auto-generated
    /// timestamp default.
    pub fn up_with_name(&self, instance_name: Option<&str>) -> Result<InfraOutput> {
        check_aws_credentials()?;

        self.config
            .validate_ubuntu_version()
            .map_err(Error::Config)?;
        self.config.validate_extensions().map_err(Error::Config)?;

        let (ssh_pub_path, ssh_priv_path) = self.config.require_ssh_key().map_err(Error::Config)?;
        if !Path::new(&ssh_pub_path).exists() {
            return Err(Error::Config(format!(
                "SSH public key not found: {ssh_pub_path}. \
                 Check `ssh_key` in your .local.toml override."
            )));
        }
        if !Path::new(&ssh_priv_path).exists() {
            return Err(Error::Config(format!(
                "SSH private key not found: {ssh_priv_path}. \
                 The private key must exist alongside the public key."
            )));
        }

        if self.config.iam_instance_profile.is_none() {
            return Err(Error::Config(
                "iam_instance_profile is required. Run `rekall infra init` \
                 to generate the IAM template, apply it in your AWS account, then set \
                 iam_instance_profile = \"rekall-profile\" in your .local.toml."
                    .to_string(),
            ));
        }

        // Without [rekall] in the config, provision.sh skips the rekall
        // build block and the instance ends up without the `rekall` binary.
        // `infra run` would silently fail later (rekall: command not found
        // exits the tmux session immediately, leaving nothing to attach to).
        // Fail loudly here instead.
        if self.config.rekall.is_none() {
            return Err(Error::Config(
                "[rekall] is required — without it provision.sh won't \
                 install the rekall CLI on the instance, and `infra run` \
                 will silently fail later. Add to your .local.toml:\n\n\
                 [rekall]\n\
                 git_url = \"git@github.com:timescale/rekall.git\"\n\
                 git_ref = \"main\""
                    .to_string(),
            ));
        }
        if self.config.pg_vectorsearch.is_none() {
            tracing::warn!(
                "[pg_vectorsearch] is not configured — the pg_vectorsearch extension won't \
                 be installed on the instance. Benchmarks that use \
                 type = \"prism\" will fail."
            );
        }

        self.state.ensure_dirs()?;

        for repo_name in &self.config.private_repo_names() {
            if !self.state.has_deploy_key(repo_name)? {
                return Err(Error::Config(format!(
                    "No deploy key for repo '{repo_name}'. \
                     Run `rekall infra init` first to generate deploy keys, \
                     then add them to your GitHub repos."
                )));
            }
        }

        let ssh_pub_key = std::fs::read_to_string(&ssh_pub_path).map_err(|e| {
            Error::Config(format!("Cannot read SSH public key '{ssh_pub_path}': {e}"))
        })?;
        let ssh_pub_key = ssh_pub_key.trim();

        let ssh_cidr = match state::detect_public_ip() {
            Ok(ip) => {
                info!("Detected public IP: {ip}");
                format!("{ip}/32")
            }
            Err(e) => {
                tracing::warn!("Cannot detect public IP ({e}), allowing SSH from 0.0.0.0/0");
                "0.0.0.0/0".to_string()
            }
        };

        let deploy_key_paths: Vec<String> = self
            .state
            .deploy_key_paths()?
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let vars = self.config.to_tfvars();
        let user_data = render_user_data(&self.config, Some(ssh_pub_key))?;

        let output = self.backend.up(
            ssh_pub_key,
            &ssh_priv_path,
            &ssh_cidr,
            &deploy_key_paths,
            &vars,
            &user_data,
            instance_name,
        )?;

        info!("Verifying PostgreSQL is ready on {}...", output.public_ip);
        self.wait_for_pg(&output.public_ip, Duration::from_secs(120))?;

        Ok(output)
    }

    pub fn ensure_deploy_keys(
        &self,
        repo_names: &[&str],
        regenerate: bool,
    ) -> Result<Vec<state::DeployKey>> {
        self.state.ensure_dirs()?;

        let mut keys = Vec::new();
        for repo_name in repo_names {
            let key = self.state.ensure_deploy_key(repo_name, regenerate)?;
            keys.push(key);
        }
        Ok(keys)
    }

    pub fn provision(&self) -> Result<()> {
        let status = self.backend.status()?;
        if !status.running {
            return Err(Error::InfraNotRunning);
        }

        let ip = status.public_ip.as_ref().ok_or(Error::InfraNotRunning)?;
        let (_, ssh_priv) = self.config.require_ssh_key().map_err(Error::Config)?;

        let deploy_key_paths: Vec<String> = self
            .state
            .deploy_key_paths()?
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        ssh_provision(ip, &ssh_priv, &self.config, &self.state, &deploy_key_paths)
    }

    pub fn down(&self) -> Result<()> {
        self.backend.down()
    }

    pub fn status(&self) -> Result<InfraStatus> {
        let mut status = self.backend.status()?;
        if let Some(ref ip) = status.public_ip {
            status.pg_ready = self.check_pg_ready(ip);
        }
        Ok(status)
    }

    pub fn ssh_command(&self) -> Result<String> {
        let outputs = self.backend.outputs()?;
        let ip = outputs.get("public_ip").ok_or(Error::InfraNotRunning)?;
        let (_, ssh_priv) = self.config.require_ssh_key().map_err(Error::Config)?;

        Ok(format!(
            "ssh -i {ssh_priv} -o StrictHostKeyChecking=no ubuntu@{ip}",
        ))
    }

    pub fn dsn(&self) -> Result<String> {
        let outputs = self.backend.outputs()?;
        outputs.get("dsn").cloned().ok_or(Error::InfraNotRunning)
    }

    fn wait_for_pg(&self, ip: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let (_, ssh_priv) = self.config.require_ssh_key().map_err(Error::Config)?;

        info!(
            "Waiting for SSH + PostgreSQL (timeout: {}s)",
            timeout.as_secs()
        );

        loop {
            if Instant::now() > deadline {
                return Err(Error::InfraTimeout {
                    timeout_secs: timeout.as_secs(),
                });
            }

            let ready = check_pg_via_ssh(ip, Path::new(&ssh_priv), self.config.pg_port);
            if ready {
                info!("PostgreSQL is ready on {ip}:{}", self.config.pg_port);
                return Ok(());
            }

            std::thread::sleep(Duration::from_secs(15));
        }
    }

    fn check_pg_ready(&self, ip: &str) -> bool {
        let ssh_priv = self.config.ssh_private_key_path();
        check_pg_via_ssh(ip, Path::new(&ssh_priv), self.config.pg_port)
    }
}

fn ssh_run(ip: &str, ssh_priv_path: &str, cmd: &str) -> Result<()> {
    let status = Command::new("ssh")
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "ConnectTimeout=10",
            "-i",
            ssh_priv_path,
            &format!("ubuntu@{ip}"),
            cmd,
        ])
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|e| Error::Config(format!("SSH failed to start: {e}")))?;

    if !status.success() {
        return Err(Error::Config(format!(
            "Remote command failed on {ip} (see output above)"
        )));
    }
    Ok(())
}

fn ssh_wait(ip: &str, ssh_priv_path: &str) -> Result<()> {
    info!("Waiting for SSH to become available...");
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() > deadline {
            return Err(Error::InfraTimeout { timeout_secs: 300 });
        }
        if ssh_run(ip, ssh_priv_path, "true").is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(10));
    }
}

fn scp_file(ip: &str, ssh_priv_path: &str, local: &str, remote: &str) -> Result<()> {
    let status = Command::new("scp")
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-i",
            ssh_priv_path,
            local,
            &format!("ubuntu@{ip}:{remote}"),
        ])
        .status()
        .map_err(|e| Error::Config(format!("SCP failed: {e}")))?;

    if !status.success() {
        return Err(Error::Config(format!(
            "Failed to upload {local} to {ip}:{remote}"
        )));
    }
    Ok(())
}

pub fn ssh_provision(
    ip: &str,
    ssh_priv_path: &str,
    config: &InfraConfig,
    state: &StateManager,
    deploy_key_paths: &[String],
) -> Result<()> {
    ssh_wait(ip, ssh_priv_path)?;

    info!("Waiting for cloud-init to complete...");
    // With the fault-tolerant install tail, a cloud-init failure means the
    // state-critical bootstrap (PG install, tuning, roles) did not finish —
    // the box is NOT usable. Pull the diagnostics into the console (status
    // detail + bootstrap log tail) and fail `infra up` with a pointer to
    // them, instead of surfacing a bare ssh exit code.
    if ssh_run(ip, ssh_priv_path, "cloud-init status --wait").is_err() {
        let _ = ssh_run(ip, ssh_priv_path, "cloud-init status --long || true");
        let _ = ssh_run(
            ip,
            ssh_priv_path,
            "sudo tail -n 40 /var/log/rekall-setup.log || true",
        );
        return Err(Error::Config(format!(
            "cloud-init failed on {ip}: instance bootstrap did not complete \
             (status and /var/log/rekall-setup.log tail printed above)"
        )));
    }

    // The bootstrap's fault-tolerant tail records failed installs in
    // /tmp/rekall-setup-failures instead of aborting cloud-init (the
    // server tuning/roles are already applied by then). Surface them
    // loudly but continue: the box is usable, and a missing extension
    // fails fast at CREATE EXTENSION with a clear error.
    if ssh_run(
        ip,
        ssh_priv_path,
        "test ! -f /tmp/rekall-setup-failures || { \
           echo \"BOOTSTRAP INSTALL FAILURES:$(cat /tmp/rekall-setup-failures)\"; \
           exit 1; }",
    )
    .is_err()
    {
        tracing::warn!(
            "some bootstrap installs failed on {ip} — components listed \
             above; details in /var/log/rekall-setup.log on the instance"
        );
    }

    // Upload deploy keys
    if !deploy_key_paths.is_empty() {
        info!("Uploading deploy keys");
        ssh_run(ip, ssh_priv_path, "mkdir -p ~/.ssh /tmp/deploy-keys")?;

        for key_path in deploy_key_paths {
            let name = Path::new(key_path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            scp_file(
                ip,
                ssh_priv_path,
                key_path,
                &format!("/tmp/deploy-keys/{name}"),
            )?;
        }

        ssh_run(
            ip,
            ssh_priv_path,
            "ssh-keyscan github.com >> ~/.ssh/known_hosts 2>/dev/null && \
             sudo bash -c 'install -m 700 -d /root/.ssh && ssh-keyscan github.com >> /root/.ssh/known_hosts 2>/dev/null' && \
             for key in /tmp/deploy-keys/*; do \
               [ -f \"$key\" ] && name=$(basename \"$key\") && \
               cp \"$key\" \"$HOME/.ssh/$name\" && chmod 600 \"$HOME/.ssh/$name\" && \
               sudo cp \"$key\" \"/root/.ssh/$name\" && sudo chmod 600 \"/root/.ssh/$name\"; \
             done && rm -rf /tmp/deploy-keys",
        )?;
    }

    // Upload and run provisioning script
    let has_vectorchord = config.extensions.contains(&"vectorchord".to_string());
    let has_pg_pinecone = config.extensions.contains(&"pg_pinecone".to_string());
    let has_builds = config.pg_vectorsearch.is_some()
        || config.rekall.is_some()
        || has_vectorchord
        || has_pg_pinecone;
    if has_builds {
        info!("Uploading provisioning script");

        let provision_script = include_str!("../../../infra/provision.sh");
        let tmp_script = state.base_dir().join("provision.sh");
        std::fs::write(&tmp_script, provision_script)
            .map_err(|e| Error::Config(format!("Cannot write provision script: {e}")))?;

        scp_file(
            ip,
            ssh_priv_path,
            tmp_script.to_str().unwrap(),
            "/tmp/provision.sh",
        )?;
        let _ = std::fs::remove_file(&tmp_script);

        let mut env_vars = vec![
            format!("PG_VERSION={}", config.pg_version),
            format!("PG_PORT={}", config.pg_port),
        ];
        if let Some(ref m) = config.pg_vectorsearch {
            env_vars.push(format!("PG_VECTORSEARCH_GIT_URL={}", m.git_url));
            env_vars.push(format!("PG_VECTORSEARCH_GIT_REF={}", m.git_ref));
        }
        if let Some(ref r) = config.rekall {
            env_vars.push(format!("REKALL_GIT_URL={}", r.git_url));
            env_vars.push(format!("REKALL_GIT_REF={}", r.git_ref));
        }
        if has_vectorchord {
            env_vars.push("VECTORCHORD=1".to_string());
            env_vars.push(format!(
                "VECTORCHORD_VERSION={}",
                config.vectorchord_version()
            ));
        }
        if has_pg_pinecone {
            env_vars.push("PG_PINECONE=1".to_string());
            env_vars.push(format!(
                "PG_PINECONE_VERSION={}",
                config.pg_pinecone_version()
            ));
        }

        info!("Running provisioning script");
        let env_str = env_vars.join(" ");
        ssh_run(
            ip,
            ssh_priv_path,
            &format!("chmod +x /tmp/provision.sh && {env_str} /tmp/provision.sh"),
        )?;
    }

    Ok(())
}

fn check_pg_via_ssh(ip: &str, key_path: &Path, pg_port: u16) -> bool {
    let output = Command::new("ssh")
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "BatchMode=yes",
            "-i",
            key_path.to_str().unwrap_or(""),
            &format!("ubuntu@{ip}"),
            &format!("pg_isready -p {pg_port}"),
        ])
        .output();

    match output {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

fn check_aws_credentials() -> Result<()> {
    let has_env = std::env::var("AWS_ACCESS_KEY_ID").is_ok()
        || std::env::var("AWS_PROFILE").is_ok()
        || std::env::var("AWS_SHARED_CREDENTIALS_FILE").is_ok();

    if has_env {
        return Ok(());
    }

    let home = dirs::home_dir().unwrap_or_default();
    let creds_file = home.join(".aws").join("credentials");
    let config_file = home.join(".aws").join("config");

    if creds_file.exists() || config_file.exists() {
        return Ok(());
    }

    Err(Error::AwsCredentialsMissing)
}

#[derive(Debug)]
pub struct InfraInstance {
    pub name: String,
    pub status: InfraStatus,
}

/// Render the cloud-init user-data script for an instance.
/// Single source of truth shared by both the aws_native and terraform backends.
///
/// `local_pub_key`, if provided, is the launcher's own SSH public key
/// (the same one uploaded as the EC2 key pair). It is appended to the
/// team's `infra/authorized_keys` list, unless the same key body is
/// already in there.
pub fn render_user_data(config: &InfraConfig, local_pub_key: Option<&str>) -> Result<String> {
    const TEMPLATE: &str = include_str!("../../../infra/user-data.sh.j2");

    let mut env = minijinja::Environment::new();
    env.add_template("user-data", TEMPLATE)
        .map_err(|e| Error::Config(format!("user-data template parse error: {e}")))?;

    let authorized_keys = collect_authorized_keys(local_pub_key);

    // AWS AMD instances have 'a' in the family suffix (m8a, r7ad, m8azn, ...).
    // Split on '.' to get the prefix, then check for 'a' after the digit.
    let is_amd = config
        .instance_type
        .split('.')
        .next()
        .map(|prefix| {
            let after_digit = prefix.trim_start_matches(|c: char| c.is_ascii_alphabetic());
            let after_digit = after_digit.trim_start_matches(|c: char| c.is_ascii_digit());
            after_digit.starts_with('a')
        })
        .unwrap_or(false);

    let ctx = minijinja::context! {
        pg_version => config.pg_version,
        pg_port => config.pg_port,
        extensions => &config.extensions,
        pg_config => config.effective_pg_config(),
        pgvector_version => config.pgvector_version(),
        huge_pages => config.huge_pages,
        nr_hugepages => config.nr_huge_pages().unwrap_or(0),
        swap_gb => config.swap_gb,
        pg_on_instance_store => config.pg_on_instance_store,
        is_amd => is_amd,
        authorized_keys => authorized_keys,
    };

    env.get_template("user-data")
        .map_err(|e| Error::Config(format!("user-data template lookup: {e}")))?
        .render(ctx)
        .map_err(|e| Error::Config(format!("user-data render error: {e}")))
}

/// Load the committed `infra/authorized_keys` and optionally append the
/// launcher's local key (deduped on `(type, body)`, ignoring the comment).
fn collect_authorized_keys(local_pub_key: Option<&str>) -> Vec<String> {
    const PERMANENT: &str = include_str!("../../../infra/authorized_keys");

    let mut keys: Vec<String> = PERMANENT
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect();

    if let Some(local) = local_pub_key {
        let local = local.trim();
        if let Some(local_body) = ssh_key_body(local) {
            let already_present = keys
                .iter()
                .filter_map(|k| ssh_key_body(k))
                .any(|b| b == local_body);
            if !already_present {
                keys.push(local.to_string());
            }
        }
    }
    keys
}

/// Extract `(type, base64-body)` from an OpenSSH public key line.
/// Returns None for blank lines, comments, or malformed input.
fn ssh_key_body(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.split_whitespace();
    let key_type = parts.next()?;
    let body = parts.next()?;
    Some((key_type.to_string(), body.to_string()))
}

/// List instances from local state files.
pub fn list_instances_local() -> Result<Vec<InfraInstance>> {
    let names = state::list_infra_names()?;
    let mut instances = Vec::new();

    for name in names {
        let config = InfraConfig {
            name_prefix: name.clone(),
            ..Default::default()
        };
        let mgr = InfraManager::new(&config)?;
        let status = mgr.status()?;
        instances.push(InfraInstance { name, status });
    }

    Ok(instances)
}

/// Look up a single AWS instance by its instance-id and return an
/// `InfraInstance`. Returns InfraNotRunning if not found.
///
/// Used by `attach`/`ssh`/`dsn` with `--instance-id` to bypass local
/// state — useful when multiple instances exist under the same
/// name_prefix, or when you're on a different machine than the one
/// that launched it.
pub fn find_instance(region: &str, instance_id: &str) -> Result<InfraInstance> {
    let config = InfraConfig {
        region: region.to_string(),
        ..Default::default()
    };
    let backend = AwsNativeBackend::new(std::env::current_dir().unwrap_or_default(), &config);
    let d = backend
        .find_instance_by_id(instance_id)?
        .ok_or(Error::InfraNotRunning)?;
    Ok(InfraInstance {
        name: d.suite.clone(),
        status: InfraStatus {
            running: d.state == "running",
            public_ip: d.public_ip,
            instance_id: Some(d.instance_id),
            instance_type: Some(d.instance_type),
            instance_name: d.name,
            dsn: None,
            iam_profile: None,
            pg_ready: false,
        },
    })
}

/// Terminate a specific instance by AWS instance-id. Used by
/// `infra down --instance-id`. Only tears down the instance; leaves
/// the security group and key pair in place because they're scoped
/// by name_prefix and may be shared with other instances under the
/// same suite.
pub fn terminate_instance(region: &str, instance_id: &str) -> Result<()> {
    let config = InfraConfig {
        region: region.to_string(),
        ..Default::default()
    };
    let backend = AwsNativeBackend::new(std::env::current_dir().unwrap_or_default(), &config);
    backend.terminate_by_id(instance_id)
}

/// Result of an in-place instance resize.
pub struct ResizeOutcome {
    pub instance_id: String,
    pub old_type: String,
    pub new_type: String,
    /// AWS reassigns the public IP across the stop/start.
    pub new_ip: String,
}

/// Resize a running instance to `target`'s instance type in place,
/// preserving the EBS volume (and any index built on it).
///
/// PostgreSQL is re-tuned for the new size *before* the box is stopped:
/// the GUCs from `target.effective_pg_config()` are written via
/// `ALTER SYSTEM` (postgresql.auto.conf, which overrides the cloud-init
/// postgresql.conf), so the smaller box boots with a `shared_buffers`
/// etc. that fit its RAM. The change is then made via stop -> modify
/// instance type -> start, and the box is re-tagged + adopted under the
/// target config's name_prefix.
///
/// Architecture must match (no x86<->Graviton — that needs a different
/// AMI). The public IP changes (stop/start reassigns it).
pub fn resize_instance_to(
    target: &InfraConfig,
    instance_id: &str,
    current_ip: &str,
    current_type: &str,
    instance_name: Option<String>,
) -> Result<ResizeOutcome> {
    // Arch guard: an in-place type change can't cross architectures.
    let current_cfg = InfraConfig {
        instance_type: current_type.to_string(),
        ..Default::default()
    };
    if current_cfg.is_graviton() != target.is_graviton() {
        return Err(Error::Config(format!(
            "Cannot resize across architectures in place: {current_type} and \
             {} differ (x86 vs Graviton need different AMIs). Launch a fresh \
             instance instead.",
            target.instance_type
        )));
    }

    let (_, ssh_priv) = target.require_ssh_key().map_err(Error::Config)?;

    // Spot guard: a Spot instance cannot be stopped or have its type
    // changed, which is exactly what the resize below does (stop -> modify
    // type -> start). Fail fast here, before any PostgreSQL re-tuning, with
    // a message that says why and what to do instead.
    let state = StateManager::new(&target.name_prefix)?;
    let backend = AwsNativeBackend::new(state.base_dir().to_path_buf(), target);
    if backend.is_spot_instance(instance_id)? {
        return Err(Error::Config(format!(
            "Cannot resize {instance_id}: it is a Spot instance, and Spot \
             instances cannot be stopped or have their instance type changed \
             (resize does stop -> modify type -> start). Launch the resize \
             ladder's instances on-demand instead -- set `spot = false` in the \
             target config, or start from an on-demand box and resize down. \
             (Spot is the default; on-demand is required only for the \
             resize-based ladder.)"
        )));
    }

    // 1. Re-tune PostgreSQL for the target size while the box is still
    //    up. One ALTER SYSTEM per GUC (separate `-c` options run as
    //    independent statements — ALTER SYSTEM can't run in a multi-
    //    statement transaction block). The values take effect on the
    //    restart that the resize stop/start performs.
    let pg = target.effective_pg_config();
    let mut keys: Vec<&String> = pg.keys().collect();
    keys.sort();
    let mut psql = format!(
        "sudo -u postgres psql -p {} -v ON_ERROR_STOP=1",
        target.pg_port
    );
    for k in &keys {
        psql.push_str(&format!(" -c \"ALTER SYSTEM SET {} = '{}'\"", k, pg[*k]));
    }
    info!(
        "Re-tuning PostgreSQL for {} ({} settings) before resize...",
        target.instance_type,
        pg.len()
    );
    ssh_run(current_ip, &ssh_priv, &psql)?;

    // 2. Change the instance type in place (preserves the EBS volume).
    //    Reuses the backend built for the Spot guard above.
    let new_ip = backend.resize_instance(instance_id, &target.instance_type)?;

    // 3. Bring the volume in line with the target's disk spec, so a rung is
    //    a coherent deployment rather than a new instance size bolted onto
    //    the previous rung's storage. Online; a failure here is logged and
    //    not fatal, since the resize itself has already succeeded.
    if let Err(e) = backend.sync_root_volume_perf(instance_id) {
        tracing::warn!(
            "Could not apply the target's disk_iops/disk_throughput: {e}. The \
             volume keeps its previous settings -- the run will record what it \
             actually used."
        );
    }

    // 4. Adopt the box under the target config: re-tag the suite and
    //    persist local state so `-c <target>` resolves it afterward.
    let _ = backend.retag_suite(instance_id);
    let _ = backend.adopt(instance_id, &new_ip, &target.instance_type, instance_name);

    // 5. Wait for SSH + PostgreSQL on the new IP.
    ssh_wait(&new_ip, &ssh_priv)?;
    info!("Waiting for PostgreSQL to accept connections on the resized instance...");
    let key_path = Path::new(&ssh_priv);
    let deadline = Instant::now() + Duration::from_secs(300);
    while !check_pg_via_ssh(&new_ip, key_path, target.pg_port) {
        if Instant::now() > deadline {
            return Err(Error::InfraTimeout { timeout_secs: 300 });
        }
        std::thread::sleep(Duration::from_secs(10));
    }

    Ok(ResizeOutcome {
        instance_id: instance_id.to_string(),
        old_type: current_type.to_string(),
        new_type: target.instance_type.clone(),
        new_ip,
    })
}

/// Look up an instance by its Name tag (as set via `infra up --name`
/// or the timestamp default). Errors if no match or multiple matches
/// — collisions are surfaced with the matching instance-ids so the
/// caller can pick one with --instance-id.
pub fn find_instance_by_name(region: &str, name: &str) -> Result<InfraInstance> {
    let config = InfraConfig {
        region: region.to_string(),
        ..Default::default()
    };
    let backend = AwsNativeBackend::new(std::env::current_dir().unwrap_or_default(), &config);
    let matches = backend.find_instances_by_name(name)?;
    match matches.len() {
        0 => Err(Error::Config(format!(
            "No instance with Name tag '{name}' found in {region}."
        ))),
        1 => {
            let d = matches.into_iter().next().unwrap();
            Ok(InfraInstance {
                name: d.suite.clone(),
                status: InfraStatus {
                    running: d.state == "running",
                    public_ip: d.public_ip,
                    instance_id: Some(d.instance_id),
                    instance_type: Some(d.instance_type),
                    instance_name: d.name,
                    dsn: None,
                    iam_profile: None,
                    pg_ready: false,
                },
            })
        }
        _ => {
            let ids: Vec<String> = matches.iter().map(|d| d.instance_id.clone()).collect();
            Err(Error::Config(format!(
                "Multiple instances with Name tag '{name}' in {region}: {}.\n\
                 Disambiguate with --instance-id <id>.",
                ids.join(", ")
            )))
        }
    }
}

/// Discover all rekall instances in AWS by tag, regardless of local state.
pub fn list_instances_aws(region: &str) -> Result<Vec<InfraInstance>> {
    let config = InfraConfig {
        region: region.to_string(),
        ..Default::default()
    };
    let backend = AwsNativeBackend::new(std::env::current_dir().unwrap_or_default(), &config);
    let discovered = backend.discover_instances()?;

    Ok(discovered
        .into_iter()
        .map(|d| InfraInstance {
            name: d.suite.clone(),
            status: InfraStatus {
                running: d.state == "running",
                public_ip: d.public_ip,
                instance_id: Some(d.instance_id),
                instance_type: Some(d.instance_type),
                instance_name: d.name,
                dsn: None,
                iam_profile: None,
                pg_ready: false,
            },
        })
        .collect())
}

pub fn iam_terraform_template(config: &InfraConfig) -> String {
    // When a datasets bucket is named, also grant the results bucket (publish
    // target) so the role can write the run archive. With no bucket named the
    // policy is already `*`, which covers both.
    let results_bucket = config.results_bucket();
    let s3_resources = if let Some(ref bucket) = config.s3_bucket {
        format!(
            r#"      "arn:aws:s3:::{bucket}",
      "arn:aws:s3:::{bucket}/*",
      "arn:aws:s3:::{results_bucket}",
      "arn:aws:s3:::{results_bucket}/*""#
        )
    } else {
        r#"      "arn:aws:s3:::*""#.to_string()
    };

    format!(
        r#"# Rekall IAM resources — review and apply with `terraform apply`
#
# This creates:
#   1. An IAM role that EC2 instances can assume
#   2. An S3 read policy for dataset access
#   3. An instance profile that rekall references in [infra] config
#
# After applying, set in your .local.toml:
#   [infra]
#   iam_instance_profile = "rekall-profile"

resource "aws_iam_role" "rekall" {{
  name = "rekall-role"

  assume_role_policy = jsonencode({{
    Version = "2012-10-17"
    Statement = [{{
      Action = "sts:AssumeRole"
      Effect = "Allow"
      Principal = {{
        Service = "ec2.amazonaws.com"
      }}
    }}]
  }})
}}

resource "aws_iam_role_policy" "rekall_s3" {{
  name = "rekall-s3-read"
  role = aws_iam_role.rekall.id

  policy = jsonencode({{
    Version = "2012-10-17"
    Statement = [{{
      Effect = "Allow"
      Action = [
        "s3:GetObject",
        "s3:ListBucket",
        "s3:HeadObject",
        "s3:PutObject",
        "s3:CreateBucket"
      ]
      Resource = [
{s3_resources}
      ]
    }}, {{
      # See the CloudFormation template: the run records the provisioned
      # IOPS/throughput of the volume backing PGDATA, which the guest cannot
      # see for itself. Read-only describe.
      Effect = "Allow"
      Action = ["ec2:DescribeVolumes"]
      Resource = "*"
    }}]
  }})
}}

resource "aws_iam_instance_profile" "rekall" {{
  name = "rekall-profile"
  role = aws_iam_role.rekall.name
}}

# IAM user for running rekall infra commands (minimal EC2 permissions)
resource "aws_iam_user" "rekall_operator" {{
  name = "rekall-operator"
}}

resource "aws_iam_user_policy" "rekall_operator" {{
  name = "rekall-operator-ec2"
  user = aws_iam_user.rekall_operator.name

  policy = jsonencode({{
    Version = "2012-10-17"
    Statement = [
      {{
        Effect = "Allow"
        Action = [
          "ec2:RunInstances",
          "ec2:TerminateInstances",
          "ec2:DescribeInstances",
          "ec2:DescribeImages",
          "ec2:CreateSecurityGroup",
          "ec2:DeleteSecurityGroup",
          "ec2:AuthorizeSecurityGroupIngress",
          "ec2:AuthorizeSecurityGroupEgress",
          "ec2:RevokeSecurityGroupIngress",
          "ec2:RevokeSecurityGroupEgress",
          "ec2:DescribeSecurityGroups",
          "ec2:ImportKeyPair",
          "ec2:DeleteKeyPair",
          "ec2:CreateTags"
        ]
        Resource = "*"
      }},
      {{
        Effect = "Allow"
        Action = "iam:PassRole"
        Resource = aws_iam_role.rekall.arn
      }}
    ]
  }})
}}

resource "aws_iam_access_key" "rekall_operator" {{
  user = aws_iam_user.rekall_operator.name
}}

output "instance_profile_name" {{
  value = aws_iam_instance_profile.rekall.name
}}

output "role_arn" {{
  value = aws_iam_role.rekall.arn
}}

output "operator_access_key_id" {{
  value = aws_iam_access_key.rekall_operator.id
}}

output "operator_secret_access_key" {{
  value     = aws_iam_access_key.rekall_operator.secret
  sensitive = true
}}
"#
    )
}

pub fn iam_cloudformation_template(config: &InfraConfig) -> String {
    // Also grant the results bucket (publish target) so the role can write the
    // run archive; `*` already covers it when no bucket is named.
    let results_bucket = config.results_bucket();
    let s3_resources = if let Some(ref bucket) = config.s3_bucket {
        format!(
            r#"                Resource:
                  - "arn:aws:s3:::{bucket}"
                  - "arn:aws:s3:::{bucket}/*"
                  - "arn:aws:s3:::{results_bucket}"
                  - "arn:aws:s3:::{results_bucket}/*""#
        )
    } else {
        r#"                Resource:
                  - "arn:aws:s3:::*""#
            .to_string()
    };

    format!(
        r#"# Rekall IAM resources
#
# Apply via AWS Console:
#   CloudFormation -> Create stack -> With new resources -> Upload this file
#
# Or via CLI:
#   aws cloudformation deploy \
#     --template-file rekall-iam.yaml \
#     --stack-name rekall-iam \
#     --capabilities CAPABILITY_NAMED_IAM
#
# After applying, set in your .local.toml:
#   [infra]
#   iam_instance_profile = "rekall-profile"

AWSTemplateFormatVersion: "2010-09-09"
Description: IAM resources for Rekall benchmark instances

Resources:
  RekallRole:
    Type: AWS::IAM::Role
    DeletionPolicy: Delete
    Properties:
      RoleName: rekall-role
      AssumeRolePolicyDocument:
        Version: "2012-10-17"
        Statement:
          - Effect: Allow
            Principal:
              Service: ec2.amazonaws.com
            Action: sts:AssumeRole
      Policies:
        - PolicyName: rekall-s3-read
          PolicyDocument:
            Version: "2012-10-17"
            Statement:
              - Effect: Allow
                Action:
                  - "s3:GetObject"
                  - "s3:ListBucket"
                  - "s3:HeadObject"
                  - "s3:PutObject"
                  - "s3:CreateBucket"
{s3_resources}
        # Lets a benchmark record the storage it actually ran on. The
        # provisioned IOPS/throughput of the volume backing PGDATA is not
        # visible from inside the guest -- sysfs exposes the device but not
        # what EBS was configured to deliver -- so the run asks EC2. Read
        # only, and scoped to describe: without it every published run shows
        # "provisioned IOPS unavailable", which is how a benchmark ends up
        # silently measuring a 3000-IOPS baseline it believed was 16000.
        - PolicyName: rekall-ec2-describe-volumes
          PolicyDocument:
            Version: "2012-10-17"
            Statement:
              - Effect: Allow
                Action:
                  - "ec2:DescribeVolumes"
                Resource: "*"

  RekallInstanceProfile:
    Type: AWS::IAM::InstanceProfile
    DeletionPolicy: Delete
    Properties:
      InstanceProfileName: rekall-profile
      Roles:
        - !Ref RekallRole

  # IAM user for running rekall infra commands (minimal EC2 permissions)
  RekallOperator:
    Type: AWS::IAM::User
    DeletionPolicy: Delete
    Properties:
      UserName: rekall-operator
      Policies:
        - PolicyName: rekall-operator-ec2
          PolicyDocument:
            Version: "2012-10-17"
            Statement:
              - Effect: Allow
                Action:
                  - "ec2:RunInstances"
                  - "ec2:TerminateInstances"
                  - "ec2:DescribeInstances"
                  - "ec2:DescribeImages"
                  - "ec2:CreateSecurityGroup"
                  - "ec2:DeleteSecurityGroup"
                  - "ec2:AuthorizeSecurityGroupIngress"
                  - "ec2:AuthorizeSecurityGroupEgress"
                  - "ec2:RevokeSecurityGroupIngress"
                  - "ec2:RevokeSecurityGroupEgress"
                  - "ec2:DescribeSecurityGroups"
                  - "ec2:ImportKeyPair"
                  - "ec2:DeleteKeyPair"
                  - "ec2:CreateTags"
                Resource: "*"
              - Effect: Allow
                Action: "iam:PassRole"
                Resource: !GetAtt RekallRole.Arn

  RekallOperatorAccessKey:
    Type: AWS::IAM::AccessKey
    DeletionPolicy: Delete
    Properties:
      UserName: !Ref RekallOperator

Outputs:
  InstanceProfileName:
    Value: !Ref RekallInstanceProfile
  RoleArn:
    Value: !GetAtt RekallRole.Arn
  OperatorAccessKeyId:
    Value: !Ref RekallOperatorAccessKey
  OperatorSecretAccessKey:
    Value: !GetAtt RekallOperatorAccessKey.SecretAccessKey
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_user_data_minimal() {
        let cfg = InfraConfig {
            instance_type: "r7g.2xlarge".to_string(),
            extensions: vec!["pgvector".to_string()],
            ..Default::default()
        };
        let out = render_user_data(&cfg, None).expect("template renders");

        assert!(out.starts_with("#!/bin/bash"));
        assert!(out.contains("apt-get install -y postgresql-18 postgresql-server-dev-18"));
        assert!(out.contains("try_step \"pgvector v0.8.2\" install_pgvector"));
        assert!(out.contains("shared_buffers = '16GB'"));
        assert!(out.contains("port = 5432"));
        // Core dumps enabled for crash debugging (package installed with the
        // base toolchain; configuration applied before extension installs).
        assert!(out.contains("systemd-coredump"));
        assert!(out.contains("LimitCORE=infinity"));
        assert!(out.contains("postgresql@18-main.service.d/coredump.conf"));
        // State-critical setup (tuning, roles, core dumps) must precede the
        // network-fragile extension installs, so a failed install loses only
        // that component — never the server configuration.
        let pgvector_pos = out.find("try_step \"pgvector").unwrap();
        assert!(out.find("shared_buffers = '16GB'").unwrap() < pgvector_pos);
        assert!(out.find("CREATE USER rekall").unwrap() < pgvector_pos);
        assert!(out.find("LimitCORE=infinity").unwrap() < pgvector_pos);
        // Fragile installs are fault-tolerant: failures are recorded for
        // rekall to surface, not fatal to the whole bootstrap.
        assert!(out.contains("rekall-setup-failures"));
        // Background apt upgrades disabled so they can't restart PG mid-run.
        assert!(out.contains("apt-daily-upgrade.timer"));
        assert!(out.contains("99rekall-no-auto-upgrade"));
        // Huge pages off by default — block should be omitted entirely.
        assert!(!out.contains("vm.nr_hugepages"));
        assert!(!out.contains("disable-thp.service"));
    }

    #[test]
    fn render_user_data_huge_pages() {
        let cfg = InfraConfig {
            instance_type: "r7g.2xlarge".to_string(),
            huge_pages: true,
            ..Default::default()
        };
        let out = render_user_data(&cfg, None).expect("template renders");

        assert!(out.contains("vm.nr_hugepages = 9011"));
        assert!(out.contains("disable-thp.service"));
        assert!(out.contains("huge_pages = 'on'"));
    }

    #[test]
    fn render_user_data_no_pgvector() {
        let cfg = InfraConfig {
            instance_type: "r7g.2xlarge".to_string(),
            extensions: vec![],
            ..Default::default()
        };
        let out = render_user_data(&cfg, None).expect("template renders");
        assert!(!out.contains("Installing pgvector"));
        assert!(!out.contains("git clone --branch"));
    }

    #[test]
    fn render_user_data_injects_local_key() {
        let cfg = InfraConfig::default();
        let local = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITESTKEYBODY erik@laptop";
        let out = render_user_data(&cfg, Some(local)).expect("template renders");

        assert!(out.contains("Installing team SSH keys"));
        assert!(out.contains("AAAAC3NzaC1lZDI1NTE5AAAAITESTKEYBODY"));
    }

    #[test]
    fn collect_authorized_keys_dedupes_local_against_permanent() {
        // Simulate the case: permanent file already contains the local key
        // (matching type + body, even with a different comment).
        let mut keys = vec![
            "ssh-ed25519 AAAABODY1 alice@example.com".to_string(),
            "ssh-ed25519 AAAABODY2 erik@tigerdata.com".to_string(),
        ];
        let local = "ssh-ed25519 AAAABODY2 erik@some-other-machine";

        // Direct call to dedupe helper logic
        let local_body = ssh_key_body(local).unwrap();
        let already = keys
            .iter()
            .filter_map(|k| ssh_key_body(k))
            .any(|b| b == local_body);
        assert!(
            already,
            "matching body should be detected even with different comment"
        );

        // And the same key with a different body is not a dupe
        let other = "ssh-ed25519 AAAABODY3 erik@tigerdata.com";
        let other_body = ssh_key_body(other).unwrap();
        let already2 = keys
            .iter()
            .filter_map(|k| ssh_key_body(k))
            .any(|b| b == other_body);
        assert!(!already2);
        keys.push(other.to_string());
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn render_user_data_is_valid_bash() {
        // Sanity-check that the rendered script parses cleanly. Catches
        // templating bugs like missing whitespace control or stray
        // template directives bleeding into the bash output.
        let cfg = InfraConfig {
            instance_type: "r7g.2xlarge".to_string(),
            extensions: vec!["pgvector".to_string()],
            huge_pages: true,
            ..Default::default()
        };
        let out = render_user_data(&cfg, Some("ssh-ed25519 AAAABODYFORTEST tester@example.com"))
            .expect("template renders");

        let mut tmp = std::env::temp_dir();
        tmp.push("rekall-render-check.sh");
        std::fs::write(&tmp, &out).unwrap();

        let status = std::process::Command::new("bash")
            .args(["-n", tmp.to_str().unwrap()])
            .status();
        if let Ok(s) = status {
            assert!(s.success(), "bash -n rejected the rendered script:\n{out}");
        }
        // If bash isn't on PATH, skip silently — CI may not have it.
    }

    #[test]
    fn render_user_data_instance_store_blocks_are_valid_bash() {
        // The instance-store blocks (PGDATA + swap) render conditionally;
        // exercise both together, including the shared detection function.
        let cfg = InfraConfig {
            instance_type: "i4i.xlarge".to_string(),
            extensions: vec!["pgvector".to_string()],
            pg_on_instance_store: true,
            swap_gb: 8,
            ..Default::default()
        };
        let out = render_user_data(&cfg, None).expect("template renders");
        assert!(out.contains("find_instance_store_dev"));
        assert!(
            !out.contains("nvme id-ctrl"),
            "detection must not depend on nvme-cli"
        );

        let mut tmp = std::env::temp_dir();
        tmp.push("rekall-render-check-istore.sh");
        std::fs::write(&tmp, &out).unwrap();
        let status = std::process::Command::new("bash")
            .args(["-n", tmp.to_str().unwrap()])
            .status();
        if let Ok(s) = status {
            assert!(s.success(), "bash -n rejected the rendered script:\n{out}");
        }
    }

    #[test]
    fn ssh_key_body_handles_comments_and_blanks() {
        assert_eq!(ssh_key_body(""), None);
        assert_eq!(ssh_key_body("   "), None);
        assert_eq!(ssh_key_body("# a comment"), None);
        assert_eq!(
            ssh_key_body("ssh-rsa AAAABODY user@host"),
            Some(("ssh-rsa".into(), "AAAABODY".into()))
        );
        // No comment is fine
        assert_eq!(
            ssh_key_body("ssh-ed25519 AAAABODY"),
            Some(("ssh-ed25519".into(), "AAAABODY".into()))
        );
        // Single token is malformed
        assert_eq!(ssh_key_body("ssh-rsa"), None);
    }
}
