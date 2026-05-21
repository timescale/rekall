pub mod config;
mod state;
mod terraform;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use tracing::info;

use crate::error::{Error, Result};
use config::InfraConfig;
use state::StateManager;
use terraform::TerraformRunner;

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
    runner: TerraformRunner,
}

impl InfraManager {
    pub fn new(config: &InfraConfig) -> Result<Self> {
        let state = StateManager::new(&config.name_prefix)?;
        let runner = TerraformRunner::new(state.terraform_dir());
        Ok(Self {
            config: config.clone(),
            state,
            runner,
        })
    }

    pub fn up(&self) -> Result<InfraOutput> {
        let tf_version = TerraformRunner::check_installed()?;
        info!("Terraform: {tf_version}");

        check_aws_credentials()?;

        let (ssh_pub_path, ssh_priv_path) = self.config.require_ssh_key().map_err(Error::Config)?;
        if !Path::new(&ssh_pub_path).exists() {
            return Err(Error::Config(format!(
                "SSH public key not found: {ssh_pub_path}. Check [infra].ssh_key in your .local.toml."
            )));
        }
        if !Path::new(&ssh_priv_path).exists() {
            return Err(Error::Config(format!(
                "SSH private key not found: {ssh_priv_path}. The private key must exist alongside the public key."
            )));
        }

        if self.config.iam_instance_profile.is_none() {
            return Err(Error::Config(
                "[infra].iam_instance_profile is required. Run `rekall infra iam-policy` \
                 to generate the IAM template, apply it in your AWS account, then set \
                 iam_instance_profile = \"rekall-profile\" in your .local.toml."
                    .to_string(),
            ));
        }

        self.state.ensure_dirs()?;

        for repo_name in &self.config.private_repo_names() {
            if !self.state.has_deploy_key(repo_name) {
                return Err(Error::Config(format!(
                    "No deploy key for repo '{repo_name}'. \
                     Run `rekall infra init` first to generate deploy keys, \
                     then add them to your GitHub repos."
                )));
            }
        }

        TerraformRunner::write_tf_files(&self.state.terraform_dir())?;

        let mut vars = self.config.to_tfvars();
        let user_pub_key = std::fs::read_to_string(&ssh_pub_path).map_err(|e| {
            Error::Config(format!("Cannot read SSH public key '{ssh_pub_path}': {e}"))
        })?;
        vars["ssh_public_key"] = serde_json::json!(user_pub_key.trim());
        vars["ssh_private_key_path"] = serde_json::json!(&ssh_priv_path);

        let deploy_key_paths = self.state.deploy_key_paths()?;
        if !deploy_key_paths.is_empty() {
            let paths: Vec<String> = deploy_key_paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            vars["github_ssh_key_paths"] = serde_json::json!(paths);
        }

        let ip = state::detect_public_ip();
        match ip {
            Ok(ref ip) => {
                vars["ssh_cidr"] = serde_json::json!(format!("{ip}/32"));
                info!("Detected public IP: {ip}");
            }
            Err(ref e) => {
                tracing::warn!("Cannot detect public IP ({e}), allowing SSH from 0.0.0.0/0");
                vars["ssh_cidr"] = serde_json::json!("0.0.0.0/0");
            }
        }

        self.runner.write_var_file(&vars)?;

        info!("Running terraform init");
        self.runner.init()?;

        info!("Running terraform apply (includes provisioning)");
        self.runner.apply()?;

        let outputs = self.runner.output()?;
        let public_ip = outputs
            .get("public_ip")
            .ok_or_else(|| Error::TerraformFailed {
                command: "output".to_string(),
                detail: "missing public_ip in terraform output".to_string(),
            })?
            .clone();

        info!("Verifying PostgreSQL is ready on {public_ip}...");
        self.wait_for_pg(&public_ip, Duration::from_secs(120))?;

        Ok(InfraOutput {
            public_ip: public_ip.clone(),
            instance_id: outputs.get("instance_id").cloned().unwrap_or_default(),
            instance_type: outputs.get("instance_type").cloned().unwrap_or_default(),
            dsn: outputs.get("dsn").cloned().unwrap_or_default(),
            pg_port: outputs.get("pg_port").cloned().unwrap_or_default(),
            iam_profile: outputs
                .get("iam_instance_profile")
                .cloned()
                .unwrap_or_default(),
        })
    }

    /// Ensure deploy keys exist for the given repos.
    /// Returns a list of DeployKey results.
    pub fn ensure_deploy_keys(
        &self,
        repo_names: &[&str],
        regenerate: bool,
    ) -> Result<Vec<state::DeployKey>> {
        self.state.ensure_dirs()?;

        let mut keys = Vec::new();
        for repo_name in repo_names {
            let key =
                self.state
                    .ensure_deploy_key(repo_name, &self.config.name_prefix, regenerate)?;
            keys.push(key);
        }
        Ok(keys)
    }

    pub fn down(&self) -> Result<()> {
        TerraformRunner::check_installed()?;

        if !self.runner.state_exists() {
            return Err(Error::InfraNotRunning);
        }

        info!("Running terraform destroy");
        self.runner.destroy()?;
        info!("Infrastructure destroyed");
        Ok(())
    }

    pub fn status(&self) -> Result<InfraStatus> {
        if !self.runner.state_exists() {
            return Ok(InfraStatus {
                running: false,
                public_ip: None,
                instance_id: None,
                instance_type: None,
                dsn: None,
                iam_profile: None,
                pg_ready: false,
            });
        }

        let outputs = self.runner.output()?;
        let public_ip = outputs.get("public_ip").cloned();

        let pg_ready = if let Some(ref ip) = public_ip {
            self.check_pg_ready(ip)
        } else {
            false
        };

        Ok(InfraStatus {
            running: public_ip.is_some(),
            public_ip,
            instance_id: outputs.get("instance_id").cloned(),
            instance_type: outputs.get("instance_type").cloned(),
            dsn: outputs.get("dsn").cloned(),
            iam_profile: outputs.get("iam_instance_profile").cloned(),
            pg_ready,
        })
    }

    pub fn ssh_command(&self) -> Result<String> {
        let outputs = self.runner.output()?;
        let ip = outputs.get("public_ip").ok_or(Error::InfraNotRunning)?;
        let (_, ssh_priv) = self.config.require_ssh_key().map_err(Error::Config)?;

        Ok(format!(
            "ssh -i {ssh_priv} -o StrictHostKeyChecking=no ubuntu@{ip}",
        ))
    }

    pub fn dsn(&self) -> Result<String> {
        let outputs = self.runner.output()?;
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

pub fn list_instances() -> Result<Vec<InfraInstance>> {
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

output "instance_profile_name" {{
  value = aws_iam_instance_profile.rekall.name
}}

output "role_arn" {{
  value = aws_iam_role.rekall.arn
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

Outputs:
  InstanceProfileName:
    Value: !Ref RekallInstanceProfile
  RoleArn:
    Value: !GetAtt RekallRole.Arn
"#
    )
}

pub fn resolve_infra_dsn(infra: &InfraConfig) -> Option<String> {
    let mgr = InfraManager::new(infra).ok()?;
    let status = mgr.status().ok()?;
    if status.running && status.pg_ready {
        status.dsn
    } else {
        None
    }
}
