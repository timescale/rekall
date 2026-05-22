use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use tracing::info;

use super::backend::InfraBackend;
use super::config::InfraConfig;
use super::state::StateManager;
use super::{InfraOutput, InfraStatus};
use crate::error::{Error, Result};

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct AwsState {
    instance_id: String,
    security_group_id: String,
    key_pair_name: String,
    public_ip: String,
    region: String,
    instance_type: String,
    pg_port: u16,
}

#[derive(Debug)]
pub struct DiscoveredInstance {
    pub instance_id: String,
    pub instance_type: String,
    pub public_ip: Option<String>,
    pub state: String,
    pub suite: String,
}

pub struct AwsNativeBackend {
    state_dir: PathBuf,
    config: InfraConfig,
}

impl AwsNativeBackend {
    pub fn new(state_dir: PathBuf, config: &InfraConfig) -> Self {
        Self {
            state_dir,
            config: config.clone(),
        }
    }

    fn state_file(&self) -> PathBuf {
        self.state_dir.join("aws-state.json")
    }

    fn load_state(&self) -> Result<Option<AwsState>> {
        let path = self.state_file();
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path).map_err(|e| {
            Error::Config(format!("Cannot read state file '{}': {e}", path.display()))
        })?;
        let state: AwsState = serde_json::from_str(&content)
            .map_err(|e| Error::Config(format!("Invalid state file '{}': {e}", path.display())))?;
        Ok(Some(state))
    }

    fn save_state(&self, state: &AwsState) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir).map_err(|e| {
            Error::Config(format!(
                "Cannot create state dir '{}': {e}",
                self.state_dir.display()
            ))
        })?;
        let content = serde_json::to_string_pretty(state).unwrap();
        std::fs::write(self.state_file(), content).map_err(|e| {
            Error::Config(format!(
                "Cannot write state file '{}': {e}",
                self.state_file().display()
            ))
        })?;
        Ok(())
    }

    fn delete_state(&self) {
        let _ = std::fs::remove_file(self.state_file());
    }

    fn aws_cmd(&self) -> Command {
        let mut cmd = Command::new("aws");
        cmd.args(["--region", &self.config.region]);
        cmd.args(["--output", "json"]);
        cmd
    }

    fn generate_user_data(&self) -> String {
        let pg = self.config.pg_version;
        let port = self.config.pg_port;
        let pg_config = &self.config.effective_pg_config();
        let extensions = &self.config.extensions;

        let mut script = format!(
            r#"#!/bin/bash
set -euo pipefail
exec > /var/log/rekall-setup.log 2>&1

echo "=== Rekall instance setup starting ==="
date

export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y build-essential git curl wget pkg-config \
  postgresql-common apt-transport-https ca-certificates gnupg lsb-release \
  tmux htop libhdf5-dev \
  cmake meson ninja-build clang clang-tools gdb valgrind \
  autoconf automake libtool \
  unzip libopenblas-dev

# AWS CLI v2
curl -sL "https://awscli.amazonaws.com/awscli-exe-linux-$(uname -m).zip" -o /tmp/awscliv2.zip
unzip -q /tmp/awscliv2.zip -d /tmp
/tmp/aws/install
rm -rf /tmp/aws /tmp/awscliv2.zip

/usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y
apt-get install -y postgresql-{pg} postgresql-server-dev-{pg}
"#
        );

        if extensions.contains(&"pgvector".to_string()) {
            let pgvector_ver = self.config.pgvector_version();
            script.push_str(&format!(
                r#"
echo "=== Installing pgvector {pgvector_ver} ==="
apt-get install -y postgresql-{pg}-pgvector || {{
  echo "pgvector apt package not available, building from source"
  cd /tmp
  git clone --branch {pgvector_ver} --depth 1 https://github.com/pgvector/pgvector.git
  cd pgvector
  make PG_CONFIG=/usr/lib/postgresql/{pg}/bin/pg_config
  make PG_CONFIG=/usr/lib/postgresql/{pg}/bin/pg_config install
}}
"#
            ));
        }

        script.push_str(&format!(
            r#"
PG_CONF="/etc/postgresql/{pg}/main/postgresql.conf"
PG_HBA="/etc/postgresql/{pg}/main/pg_hba.conf"

sed -i "s/#listen_addresses = 'localhost'/listen_addresses = '*'/" "$PG_CONF"
sed -i "s/^port = .*/port = {port}/" "$PG_CONF"
"#
        ));

        for (key, value) in pg_config {
            script.push_str(&format!("echo \"{key} = '{value}'\" >> \"$PG_CONF\"\n"));
        }

        script.push_str(&format!(
            r#"
echo "host all rekall 0.0.0.0/0 scram-sha-256" >> "$PG_HBA"

systemctl restart postgresql@{pg}-main

sudo -u postgres psql -p {port} -c "CREATE USER rekall WITH PASSWORD 'rekall' SUPERUSER"
sudo -u postgres psql -p {port} -c "CREATE DATABASE rekall OWNER rekall"

echo "=== Installing Rust ==="
sudo -u ubuntu bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'

# tmux config
sudo -u ubuntu bash -c 'cat > ~/.tmux.conf << TMUXEOF
set-window-option -g mode-keys vi
set -g history-limit 50000
set -g default-terminal "xterm-256color"
TMUXEOF'

touch /tmp/rekall-ready

echo "=== Rekall instance setup complete ==="
date
"#
        ));

        script
    }

    fn run_aws_json(&self, args: &[&str]) -> Result<serde_json::Value> {
        let mut cmd = self.aws_cmd();
        cmd.args(args);
        let output = cmd.output().map_err(|e| {
            Error::Config(format!("Failed to run aws CLI: {e}. Is aws-cli installed?"))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::AwsCliFailed {
                command: format!("aws {}", args.join(" ")),
                detail: stderr.trim().to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&stdout)
            .map_err(|e| Error::Config(format!("Cannot parse AWS CLI output: {e}")))
    }

    fn find_ami(&self) -> Result<String> {
        let arch = if self.config.is_graviton() {
            "arm64"
        } else {
            "x86_64"
        };
        let ami_arch = if arch == "arm64" { "arm64" } else { "amd64" };
        let name_pattern =
            format!("ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-{ami_arch}-server-*");

        let result = self.run_aws_json(&[
            "ec2",
            "describe-images",
            "--owners",
            "099720109477",
            "--filters",
            &format!("Name=name,Values={name_pattern}"),
            "Name=architecture,Values=arm64,x86_64",
            "Name=virtualization-type,Values=hvm",
            "--query",
            "sort_by(Images, &CreationDate)[-1].ImageId",
        ])?;

        result.as_str().map(|s| s.to_string()).ok_or_else(|| {
            Error::Config(format!(
                "No Ubuntu 24.04 {arch} AMI found in {}",
                self.config.region
            ))
        })
    }

    fn create_security_group(&self, ssh_cidr: &str, name_prefix: &str) -> Result<String> {
        info!("Creating security group");
        let sg_name = format!("{name_prefix}-sg");

        let result = self.run_aws_json(&[
            "ec2",
            "create-security-group",
            "--group-name",
            &sg_name,
            "--description",
            "Rekall benchmark instance",
        ])?;

        let sg_id = result["GroupId"]
            .as_str()
            .ok_or_else(|| Error::Config("Failed to create security group".to_string()))?
            .to_string();

        // Tag security group
        self.tag_resource(&sg_id, name_prefix)?;

        // SSH ingress
        self.run_aws_json(&[
            "ec2",
            "authorize-security-group-ingress",
            "--group-id",
            &sg_id,
            "--protocol",
            "tcp",
            "--port",
            "22",
            "--cidr",
            ssh_cidr,
        ])?;

        // PG ingress
        self.run_aws_json(&[
            "ec2",
            "authorize-security-group-ingress",
            "--group-id",
            &sg_id,
            "--protocol",
            "tcp",
            "--port",
            &self.config.pg_port.to_string(),
            "--cidr",
            ssh_cidr,
        ])?;

        Ok(sg_id)
    }

    fn import_key_pair(&self, ssh_pub_key: &str, name_prefix: &str) -> Result<String> {
        info!("Importing SSH key pair");
        let key_name = format!("{name_prefix}-key");

        // Delete existing key pair if present (ignore errors)
        let _ = self.run_aws_json(&["ec2", "delete-key-pair", "--key-name", &key_name]);

        // Write public key to a temp file for fileb:// reference
        let tmp_dir = self.state_dir.join("tmp");
        std::fs::create_dir_all(&tmp_dir).ok();
        let tmp_key = tmp_dir.join("ssh_pub_key");
        std::fs::write(&tmp_key, ssh_pub_key)
            .map_err(|e| Error::Config(format!("Cannot write temp key file: {e}")))?;

        let fileb = format!("fileb://{}", tmp_key.display());
        let result = self.run_aws_json(&[
            "ec2",
            "import-key-pair",
            "--key-name",
            &key_name,
            "--public-key-material",
            &fileb,
        ]);

        // Clean up temp file
        let _ = std::fs::remove_file(&tmp_key);
        let _ = std::fs::remove_dir(&tmp_dir);

        result?;
        Ok(key_name)
    }

    fn run_instance(
        &self,
        ami_id: &str,
        sg_id: &str,
        key_name: &str,
        user_data: &str,
    ) -> Result<(String, String)> {
        info!("Launching EC2 instance ({})", self.config.instance_type);

        let user_data_b64 = base64_encode(user_data.as_bytes());
        let tag_spec = format!(
            "ResourceType=instance,Tags=[{{Key=Name,Value={0}-benchmark}},{{Key=rekall:suite,Value={0}}}]",
            self.config.name_prefix
        );
        let block_device = format!(
            "DeviceName=/dev/sda1,Ebs={{VolumeSize={},VolumeType=gp3}}",
            self.config.disk_size_gb
        );
        let profile_arg = self
            .config
            .iam_instance_profile
            .as_ref()
            .map(|p| format!("Name={p}"));

        let mut args = vec![
            "ec2",
            "run-instances",
            "--image-id",
            ami_id,
            "--instance-type",
            &self.config.instance_type,
            "--key-name",
            key_name,
            "--security-group-ids",
            sg_id,
            "--user-data",
            &user_data_b64,
            "--count",
            "1",
            "--tag-specifications",
            &tag_spec,
            "--block-device-mappings",
            &block_device,
        ];

        if let Some(ref pa) = profile_arg {
            args.push("--iam-instance-profile");
            args.push(pa);
        }

        let result = self.run_aws_json(&args)?;

        let instance_id = result["Instances"][0]["InstanceId"]
            .as_str()
            .ok_or_else(|| Error::Config("Failed to get instance ID".to_string()))?
            .to_string();

        info!("Waiting for instance to be running...");
        let _ = self.run_aws_json(&[
            "ec2",
            "wait",
            "instance-running",
            "--instance-ids",
            &instance_id,
        ]);

        let desc = self.run_aws_json(&[
            "ec2",
            "describe-instances",
            "--instance-ids",
            &instance_id,
            "--query",
            "Reservations[0].Instances[0].PublicIpAddress",
        ])?;

        let public_ip = desc
            .as_str()
            .ok_or_else(|| {
                Error::Config("Instance has no public IP. Check subnet settings.".to_string())
            })?
            .to_string();

        Ok((instance_id, public_ip))
    }

    fn run_provision(
        &self,
        ip: &str,
        ssh_priv_path: &str,
        deploy_key_paths: &[String],
    ) -> Result<()> {
        let state = StateManager::new(&self.config.name_prefix)?;
        super::ssh_provision(ip, ssh_priv_path, &self.config, &state, deploy_key_paths)
    }

    fn terminate_instance(&self, instance_id: &str) -> Result<()> {
        info!("Terminating instance {instance_id}");
        self.run_aws_json(&["ec2", "terminate-instances", "--instance-ids", instance_id])?;

        info!("Waiting for instance to terminate...");
        let _ = self.run_aws_json(&[
            "ec2",
            "wait",
            "instance-terminated",
            "--instance-ids",
            instance_id,
        ]);
        Ok(())
    }

    fn delete_security_group(&self, sg_id: &str) -> Result<()> {
        info!("Deleting security group {sg_id}");
        let _ = self.run_aws_json(&["ec2", "delete-security-group", "--group-id", sg_id]);
        Ok(())
    }

    fn delete_key_pair(&self, key_name: &str) -> Result<()> {
        info!("Deleting key pair {key_name}");
        let _ = self.run_aws_json(&["ec2", "delete-key-pair", "--key-name", key_name]);
        Ok(())
    }

    fn tag_resource(&self, resource_id: &str, name_prefix: &str) -> Result<()> {
        let _ = self.run_aws_json(&[
            "ec2",
            "create-tags",
            "--resources",
            resource_id,
            "--tags",
            &format!("Key=rekall:suite,Value={name_prefix}"),
        ]);
        Ok(())
    }

    /// Discover all rekall instances in the configured region by tag.
    pub fn discover_instances(&self) -> Result<Vec<DiscoveredInstance>> {
        let result = self.run_aws_json(&[
            "ec2",
            "describe-instances",
            "--filters",
            "Name=tag-key,Values=rekall:suite",
            "Name=instance-state-name,Values=running,stopped,pending",
            "--query",
            "Reservations[].Instances[].[InstanceId,InstanceType,PublicIpAddress,State.Name,Tags[?Key==`rekall:suite`].Value|[0]]",
        ])?;

        let mut instances = Vec::new();
        if let Some(arr) = result.as_array() {
            for item in arr {
                if let Some(fields) = item.as_array() {
                    instances.push(DiscoveredInstance {
                        instance_id: fields
                            .first()
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        instance_type: fields
                            .get(1)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        public_ip: fields
                            .get(2)
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        state: fields
                            .get(3)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        suite: fields
                            .get(4)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    });
                }
            }
        }
        Ok(instances)
    }

    fn describe_instance(&self, instance_id: &str) -> Result<Option<serde_json::Value>> {
        let result =
            self.run_aws_json(&["ec2", "describe-instances", "--instance-ids", instance_id])?;

        let instance = &result["Reservations"][0]["Instances"][0];
        if instance.is_null() {
            return Ok(None);
        }
        Ok(Some(instance.clone()))
    }
}

impl InfraBackend for AwsNativeBackend {
    fn name(&self) -> &str {
        "aws-native"
    }

    fn up(
        &self,
        ssh_pub_key: &str,
        ssh_priv_path: &str,
        ssh_cidr: &str,
        deploy_key_paths: &[String],
        _vars: &serde_json::Value,
    ) -> Result<InfraOutput> {
        let name_prefix = &self.config.name_prefix;

        let ami_id = self.find_ami()?;
        info!("Using AMI: {ami_id}");

        let sg_id = self.create_security_group(ssh_cidr, name_prefix)?;
        info!("Security group: {sg_id}");

        let key_name = self.import_key_pair(ssh_pub_key, name_prefix)?;
        info!("Key pair: {key_name}");

        let user_data = self.generate_user_data();
        let run_result = self.run_instance(&ami_id, &sg_id, &key_name, &user_data);

        let (instance_id, public_ip) = match run_result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Instance launch failed, cleaning up resources");
                let _ = self.delete_security_group(&sg_id);
                let _ = self.delete_key_pair(&key_name);
                return Err(e);
            }
        };
        info!("Instance: {instance_id} at {public_ip}");

        // Save state immediately so `infra down` can clean up if
        // subsequent steps (SSH provisioning) fail
        let state = AwsState {
            instance_id: instance_id.clone(),
            security_group_id: sg_id,
            key_pair_name: key_name,
            public_ip: public_ip.clone(),
            region: self.config.region.clone(),
            instance_type: self.config.instance_type.clone(),
            pg_port: self.config.pg_port,
        };
        self.save_state(&state)?;

        // SSH provisioning (deploy keys + builds)
        self.run_provision(&public_ip, ssh_priv_path, deploy_key_paths)?;

        let dsn = format!(
            "host={public_ip} port={} dbname=rekall user=rekall password=rekall sslmode=disable",
            self.config.pg_port
        );

        Ok(InfraOutput {
            public_ip,
            instance_id,
            instance_type: self.config.instance_type.clone(),
            dsn,
            pg_port: self.config.pg_port.to_string(),
            iam_profile: self.config.iam_instance_profile.clone().unwrap_or_default(),
        })
    }

    fn down(&self) -> Result<()> {
        let state = self.load_state()?.ok_or(Error::InfraNotRunning)?;

        self.terminate_instance(&state.instance_id)?;
        self.delete_security_group(&state.security_group_id)?;
        self.delete_key_pair(&state.key_pair_name)?;
        self.delete_state();

        info!("Infrastructure destroyed");
        Ok(())
    }

    fn status(&self) -> Result<InfraStatus> {
        let state = match self.load_state()? {
            Some(s) => s,
            None => {
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
        };

        let running = if let Ok(Some(instance)) = self.describe_instance(&state.instance_id) {
            let code = instance["State"]["Code"].as_i64().unwrap_or(0);
            code == 16 // 16 = running
        } else {
            false
        };

        let dsn = format!(
            "host={} port={} dbname=rekall user=rekall password=rekall sslmode=disable",
            state.public_ip, state.pg_port
        );

        Ok(InfraStatus {
            running,
            public_ip: Some(state.public_ip),
            instance_id: Some(state.instance_id),
            instance_type: Some(state.instance_type),
            dsn: Some(dsn),
            iam_profile: None,
            pg_ready: false,
        })
    }

    fn outputs(&self) -> Result<HashMap<String, String>> {
        let state = self.load_state()?.ok_or(Error::InfraNotRunning)?;

        let dsn = format!(
            "host={} port={} dbname=rekall user=rekall password=rekall sslmode=disable",
            state.public_ip, state.pg_port
        );

        let mut map = HashMap::new();
        map.insert("public_ip".to_string(), state.public_ip);
        map.insert("instance_id".to_string(), state.instance_id);
        map.insert("instance_type".to_string(), state.instance_type);
        map.insert("pg_port".to_string(), state.pg_port.to_string());
        map.insert("dsn".to_string(), dsn);
        Ok(map)
    }

    fn has_state(&self) -> bool {
        self.state_file().exists()
    }
}

fn base64_encode(data: &[u8]) -> String {
    use std::io::Write;
    let mut buf = Vec::new();
    let mut encoder = Base64Encoder::new(&mut buf);
    encoder.write_all(data).unwrap();
    encoder.finish();
    String::from_utf8(buf).unwrap()
}

struct Base64Encoder<'a> {
    out: &'a mut Vec<u8>,
}

impl<'a> Base64Encoder<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self { out }
    }

    fn finish(self) {}
}

const B64_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

impl std::io::Write for Base64Encoder<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut i = 0;
        while i + 2 < buf.len() {
            let b0 = buf[i] as usize;
            let b1 = buf[i + 1] as usize;
            let b2 = buf[i + 2] as usize;
            self.out.push(B64_CHARS[b0 >> 2]);
            self.out.push(B64_CHARS[((b0 & 3) << 4) | (b1 >> 4)]);
            self.out.push(B64_CHARS[((b1 & 0xf) << 2) | (b2 >> 6)]);
            self.out.push(B64_CHARS[b2 & 0x3f]);
            i += 3;
        }
        let rem = buf.len() - i;
        if rem == 2 {
            let b0 = buf[i] as usize;
            let b1 = buf[i + 1] as usize;
            self.out.push(B64_CHARS[b0 >> 2]);
            self.out.push(B64_CHARS[((b0 & 3) << 4) | (b1 >> 4)]);
            self.out.push(B64_CHARS[(b1 & 0xf) << 2]);
            self.out.push(b'=');
        } else if rem == 1 {
            let b0 = buf[i] as usize;
            self.out.push(B64_CHARS[b0 >> 2]);
            self.out.push(B64_CHARS[(b0 & 3) << 4]);
            self.out.push(b'=');
            self.out.push(b'=');
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
