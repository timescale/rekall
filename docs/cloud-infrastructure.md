# Cloud Infrastructure

Rekall can provision AWS EC2 instances with PostgreSQL pre-installed for
running benchmarks in the cloud. This is useful for large datasets that
need more memory or CPU than your local machine, and for reproducible
benchmark environments.

## Prerequisites

Install on your local machine:

- **Terraform** (>= 1.5): https://developer.hashicorp.com/terraform/install
- **AWS credentials**: run `aws configure` or set `AWS_ACCESS_KEY_ID` and
  `AWS_SECRET_ACCESS_KEY` environment variables

## What Gets Provisioned

A single EC2 instance with:

- Ubuntu 24.04 LTS (ARM for Graviton, x86_64 otherwise)
- PostgreSQL (via PGDG apt packages, latest point release)
- pgvector (via apt or built from source)
- Meerkat (built from source if configured)
- Rust toolchain + rekall (built from source if configured)
- Build tools: gcc, clang, cmake, meson, ninja, gdb, valgrind
- AWS CLI (for S3 dataset access)
- tmux, htop

The security group allows SSH and PostgreSQL access only from your
detected public IP. The instance gets an IAM role with S3 read access
for downloading datasets.

## Quick Start

```bash
# 1. Generate all setup resources and instructions
rekall infra init -c examples/cloud-benchmark.toml

# 2. Follow the printed instructions (apply IAM, add deploy key, etc.)

# 3. Provision
rekall infra up -c examples/cloud-benchmark.toml

# 4. SSH in and run benchmarks
rekall infra ssh -c examples/cloud-benchmark.toml

# 5. Tear down
rekall infra down -c examples/cloud-benchmark.toml
```

## Setup

### `rekall infra init`

This is the recommended starting point. It generates all local resources
in one step and prints instructions for what to do next:

```bash
rekall infra init -c examples/cloud-benchmark.toml
```

This generates:

1. **IAM template** (`rekall-iam.tf`) — Terraform HCL for the IAM role,
   S3 read policy, and instance profile. Use `--cloudformation` for a
   CloudFormation YAML template instead.

2. **Deploy key** — if the config references private GitHub repos
   (`git@github.com:...`), an ed25519 key pair is generated and the
   public key is printed. Add it to your GitHub repos as a read-only
   deploy key.

3. **Local config** (`.local.toml`) — a stub with placeholders for
   `ssh_key`, `iam_instance_profile`, and private git URLs. This file
   is gitignored.

After running `init`, follow the numbered instructions it prints:

1. Have an admin apply the IAM template in your AWS account (one-time)
2. Add the deploy key to GitHub repos (if using private repos)
3. Fill in the `.local.toml` with your SSH key and the instance profile name
4. Run `rekall infra up`

### IAM Setup (One-Time)

The IAM template creates minimal permissions for benchmark instances:
an IAM role that EC2 can assume, an S3 read policy for datasets, and
an instance profile.

**With Terraform:**
```bash
rekall infra iam-policy > rekall-iam.tf
# Review the template
terraform apply
```

**With CloudFormation:**
```bash
rekall infra iam-policy --cloudformation > rekall-iam.yaml
aws cloudformation deploy \
  --template-file rekall-iam.yaml \
  --stack-name rekall-iam \
  --capabilities CAPABILITY_NAMED_IAM
```

This is done once per AWS account. The benchmark user only needs EC2
permissions and `iam:PassRole` scoped to the rekall role — no
`iam:CreateRole` needed.

To scope S3 access to a specific bucket, set `s3_bucket` in the config
before generating the template:

```toml
[infra]
s3_bucket = "my-datasets-bucket"
```

### AWS Permissions

The user running `rekall infra up` needs:

- EC2: create/terminate instances, security groups, key pairs
- `iam:PassRole` scoped to the pre-created rekall role

No IAM create permissions are needed since the role is pre-created.

## Configuration

Infrastructure config lives in the `[infra]` section of your benchmark
TOML. Sensitive values (SSH key, private repo URLs, instance profile)
go in a gitignored `.local.toml` override.

### Base config (committed)

```toml
[infra]
provider = "aws"
region = "us-east-1"
instance_type = "r7g.2xlarge"    # Graviton3, 8 vCPU, 64 GB
disk_size_gb = 100               # gp3 EBS volume
pg_version = 18
extensions = ["pgvector", "meerkat"]
name_prefix = "rekall"           # resource naming + deploy key scope
```

### Local override (gitignored)

```toml
# cloud-benchmark.local.toml
[infra]
ssh_key = "~/.ssh/id_ed25519"
iam_instance_profile = "rekall-profile"

[infra.rekall]
git_url = "git@github.com:myorg/rekall.git"
git_ref = "main"

[infra.meerkat]
git_url = "git@github.com:myorg/meerkat.git"
git_ref = "feat/my-branch"
```

### PostgreSQL Auto-Configuration

PG settings are computed automatically from the instance type:

| Setting | Formula |
|---------|---------|
| `shared_buffers` | 25% of RAM |
| `maintenance_work_mem` | 10% of RAM |
| `effective_cache_size` | 75% of RAM |
| `work_mem` | 256 MB |
| `max_parallel_maintenance_workers` | 8 |

Override any setting explicitly in `[infra.pg_config]`:

```toml
[infra.pg_config]
shared_buffers = "32GB"     # override auto-computed value
huge_pages = "try"          # add extra settings
```

### Configuration Reference

| Field | Default | Description |
|-------|---------|-------------|
| `provider` | `"aws"` | Cloud provider |
| `region` | `"us-east-1"` | AWS region |
| `instance_type` | `"r7g.2xlarge"` | EC2 instance type |
| `disk_size_gb` | `100` | Root EBS volume size (gp3) |
| `disk_iops` | (default) | gp3 IOPS (default 3000) |
| `disk_throughput` | (default) | gp3 throughput MiB/s (default 125) |
| `pg_version` | `18` | PostgreSQL major version |
| `pg_port` | `5432` | PostgreSQL listen port |
| `extensions` | `[]` | Extensions to install |
| `name_prefix` | `"rekall"` | Resource naming prefix |
| `ssh_key` | (required) | Path to your SSH private key |
| `iam_instance_profile` | (required) | Pre-created IAM instance profile name |
| `s3_bucket` | (all) | S3 bucket for IAM policy scoping |

## SSH Keys

Rekall uses two separate SSH key pairs:

### 1. User key (`ssh_key`)

Your personal SSH key for logging into instances. Only the **public key**
is uploaded to AWS. The private key never leaves your machine.

```toml
# In .local.toml (never committed)
[infra]
ssh_key = "~/.ssh/id_ed25519"
```

### 2. Deploy key (auto-generated)

An ephemeral key pair for instances to access private GitHub repos. Rekall
generates it per `name_prefix` and stores it in
`.rekall/infra/{name_prefix}/deploy-key/`. The private key is uploaded to
instances via SCP over SSH (encrypted, never in Terraform state or
instance metadata).

The deploy key persists across `infra up`/`down` cycles so you don't need
to re-add it to GitHub. Regenerate with `rekall infra deploy-key --regenerate`
if you need to rotate it.

## S3 Dataset Access

Instances have an IAM role with S3 read access. Download datasets
directly on the instance:

```bash
aws s3 cp s3://my-bucket/cohere-100m.hdf5 ~/data/
```

No AWS credentials needed on the instance — the IAM role handles auth
automatically.

For cross-account S3 access, the bucket owner needs a bucket policy
that grants access to the rekall IAM role ARN. The role ARN is shown
in `rekall infra status` output.

## Multiple Instances

Use different `name_prefix` values to run multiple instances
simultaneously:

```toml
# cohere-1m.toml
[infra]
name_prefix = "rekall-1m"
instance_type = "r7g.2xlarge"

# cohere-100m.toml
[infra]
name_prefix = "rekall-100m"
instance_type = "r7g.8xlarge"
disk_size_gb = 500
```

```bash
rekall infra up -c cohere-1m.toml
rekall infra up -c cohere-100m.toml
```

List all instances:

```bash
rekall infra list
```

Target instances by name (no config file needed):

```bash
rekall infra ssh -n rekall-1m
rekall infra status -n rekall-100m
rekall infra down -n rekall-100m
```

## Instance Types

Recommended instance types for vector search benchmarks:

| Instance | vCPU | RAM | Arch | Use case |
|----------|------|-----|------|----------|
| `r7g.2xlarge` | 8 | 64 GB | Graviton3 | 1-10M vectors |
| `r7g.4xlarge` | 16 | 128 GB | Graviton3 | 10-50M vectors |
| `r8g.2xlarge` | 8 | 64 GB | Graviton4 | Best price-perf |
| `r8g.4xlarge` | 16 | 128 GB | Graviton4 | Large datasets |
| `r6i.2xlarge` | 8 | 64 GB | x86_64 | If ARM not suitable |

Graviton (ARM) instances offer better price-performance for
compute-heavy workloads like index builds and distance computations.
The `g` suffix indicates Graviton; rekall automatically selects the
matching ARM Ubuntu AMI.

## Terraform State

Terraform state is stored locally in `.rekall/infra/{name_prefix}/terraform/`.
This directory is gitignored. If the state is lost (e.g., you delete
`.rekall/`), you'll need to manually terminate the EC2 instance via the
AWS console.

## CLI Reference

```
rekall infra init [-c config.toml]         # generate all setup resources
  --cloudformation                          # IAM template as CloudFormation

rekall infra up [-c config.toml]           # provision instance
  --region <region>                         # override region
  --instance-type <type>                    # override instance type

rekall infra down [-c config.toml] [-n name]   # destroy instance
rekall infra status [-c config.toml] [-n name] # show instance status
rekall infra list                              # list all instances
rekall infra ssh [-c config.toml] [-n name]    # SSH into instance
rekall infra dsn [-c config.toml] [-n name]    # print connection string

rekall infra deploy-key [-c config.toml]   # generate/show deploy key
  --regenerate                              # force new key
rekall infra iam-policy [-c config.toml]   # print IAM template
  --cloudformation                          # as CloudFormation YAML
```

## Troubleshooting

**Terraform not found**: Install from
https://developer.hashicorp.com/terraform/install

**AWS credentials not configured**: Run `aws configure` or set
`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`.

**IAM instance profile missing**: Run `rekall infra init` and follow the
instructions to apply the IAM template.

**SSH key not found**: Set `[infra].ssh_key` in your `.local.toml` to the
path of your SSH private key.

**Deploy key not generated**: Run `rekall infra init` or
`rekall infra deploy-key`.

**Provisioning timeout**: The instance started but PostgreSQL didn't
become ready within the timeout. SSH in manually and check
`/var/log/rekall-setup.log`:

```bash
ssh -i ~/.ssh/id_ed25519 ubuntu@<ip>
sudo cat /var/log/rekall-setup.log
```

**Private repo clone failed**: Verify the deploy key's public key is
added to the GitHub repo and has read access.

**S3 access denied**: Check that the IAM role has the correct S3
permissions and the bucket policy allows the role (for cross-account).
