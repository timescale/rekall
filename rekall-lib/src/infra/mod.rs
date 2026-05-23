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
        check_aws_credentials()?;

        self.config
            .validate_ubuntu_version()
            .map_err(Error::Config)?;

        let (ssh_pub_path, ssh_priv_path) = self.config.require_ssh_key().map_err(Error::Config)?;
        if !Path::new(&ssh_pub_path).exists() {
            return Err(Error::Config(format!(
                "SSH public key not found: {ssh_pub_path}. \
                 Check [infra].ssh_key in your .local.toml."
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
                "[infra].iam_instance_profile is required. Run `rekall infra init` \
                 to generate the IAM template, apply it in your AWS account, then set \
                 iam_instance_profile = \"rekall-profile\" in your .local.toml."
                    .to_string(),
            ));
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
        let ssh_priv = match self.config.ssh_private_key_path() {
            Some(k) => k,
            None => return false,
        };
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
    ssh_run(ip, ssh_priv_path, "cloud-init status --wait")?;

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
    let has_builds = config.meerkat.is_some() || config.rekall.is_some() || has_vectorchord;
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

        let mut env_vars = vec![format!("PG_VERSION={}", config.pg_version)];
        if let Some(ref m) = config.meerkat {
            env_vars.push(format!("MEERKAT_GIT_URL={}", m.git_url));
            env_vars.push(format!("MEERKAT_GIT_REF={}", m.git_ref));
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

    let ctx = minijinja::context! {
        pg_version => config.pg_version,
        pg_port => config.pg_port,
        extensions => &config.extensions,
        pg_config => config.effective_pg_config(),
        pgvector_version => config.pgvector_version(),
        huge_pages => config.huge_pages,
        nr_hugepages => config.nr_huge_pages().unwrap_or(0),
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
                dsn: None,
                iam_profile: None,
                pg_ready: false,
            },
        })
        .collect())
}

pub fn iam_terraform_template(config: &InfraConfig) -> String {
    let s3_resources = if let Some(ref bucket) = config.s3_bucket {
        format!(
            r#"      "arn:aws:s3:::{bucket}",
      "arn:aws:s3:::{bucket}/*""#
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
        "s3:HeadObject"
      ]
      Resource = [
{s3_resources}
      ]
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
    let s3_resources = if let Some(ref bucket) = config.s3_bucket {
        format!(
            r#"                Resource:
                  - "arn:aws:s3:::{bucket}"
                  - "arn:aws:s3:::{bucket}/*""#
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
{s3_resources}

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
        assert!(out.contains("Installing pgvector v0.8.2"));
        assert!(out.contains("shared_buffers = '25GB'"));
        assert!(out.contains("port = 5432"));
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

        assert!(out.contains("vm.nr_hugepages = 14080"));
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
