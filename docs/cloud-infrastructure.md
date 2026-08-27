# Cloud Infrastructure

Rekall can provision AWS EC2 instances with PostgreSQL pre-installed for
running benchmarks in the cloud. This is useful for large datasets that
need more memory or CPU than your local machine, and for reproducible
benchmark environments.

## Prerequisites

- **AWS credentials**: run `aws configure` or set `AWS_ACCESS_KEY_ID` and
  `AWS_SECRET_ACCESS_KEY` environment variables
- **AWS CLI**: installed and on PATH (used by the default `aws` backend)

If using the Terraform backend (`provider = "terraform"`), you also need:

- **Terraform** (>= 1.5): https://developer.hashicorp.com/terraform/install

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
rekall infra init -c infra/instances/r7g.2xlarge.toml

# 2. Follow the printed instructions (apply IAM, add deploy key, etc.)

# 3. Provision
rekall infra up -c infra/instances/r7g.2xlarge.toml

# 4. SSH in and run benchmarks
rekall infra ssh -c infra/instances/r7g.2xlarge.toml

# 5. Tear down
rekall infra down -c infra/instances/r7g.2xlarge.toml
```

## Setup

### `rekall infra init`

This is the recommended starting point. It generates all local resources
in one step and prints instructions for what to do next:

```bash
rekall infra init -c infra/instances/r7g.2xlarge.toml
```

This generates:

1. **IAM template** (`rekall-iam.yaml`) — CloudFormation template for the
   IAM role, S3 read policy, and instance profile. Use `--terraform` for
   Terraform HCL instead.

2. **Deploy key** — if the config references private GitHub repos
   (`git@github.com:...`), an ed25519 key pair is generated and the
   public key is printed. Add it to your GitHub repos as a read-only
   deploy key.

3. **Machine defaults** (`~/.config/rekall/infra.toml`) — where
   `ssh_key`, `iam_instance_profile`, and the private git URLs belong.
   Written once per machine; every instance config inherits it, so a
   per-instance `.local.toml` is only needed for genuine deviations.

After running `init`, follow the numbered instructions it prints:

1. Have an admin apply the IAM template in your AWS account (one-time)
2. Add the deploy key to GitHub repos (if using private repos)
3. Fill in `~/.config/rekall/infra.toml` with your SSH key and the
   instance profile name (once per machine, not once per instance)
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
s3_bucket = "my-datasets-bucket"
```

### AWS Permissions

The user running `rekall infra up` needs:

- EC2: create/terminate instances, security groups, key pairs
- `iam:PassRole` scoped to the pre-created rekall role

No IAM create permissions are needed since the role is pre-created.

## Configuration

Infra configs are standalone TOML files (no `[infra]` wrapper) under
`infra/instances/<instance-type>.toml`. Three layers are deep-merged, each
overriding the one before:

| Layer | File | Holds |
|---|---|---|
| 1. Machine defaults | `~/.config/rekall/infra.toml` | Values identical for every box here: `ssh_key`, `iam_instance_profile`, `s3_bucket`, the meerkat/rekall git URLs |
| 2. Committed config | `infra/instances/<type>.toml` | The box itself: `instance_type`, disk, `extensions`, `pg_config` |
| 3. Local override | `<type>.local.toml` (gitignored) | Only what makes THIS box different |

Layer 1 is why most instances need no `.local.toml` at all. Reach for one
when a single box must deviate — the canonical case being a meerkat feature
branch, where `git_url` is inherited and only the ref is restated:

```toml
# infra/instances/m8g.large.local.toml
[meerkat]
git_ref = "feat/my-branch"
```

All three layers use the same flat format, so a field can be moved between
them by copy-paste. `InfraConfig` sets `deny_unknown_fields`, so a typo in
any layer fails the load with the offending key named rather than being
silently ignored. A malformed `~/.config/rekall/infra.toml` is the one
exception: it warns and is skipped, since optional defaults should not make
every instance config unloadable.

### Base config (committed — `infra/instances/r7g.2xlarge.toml`)

The committed file is self-documenting: every option you can override
in `.local.toml` appears either set or commented with an explanation.
Skeleton:

```toml
provider      = "aws"
region        = "us-east-1"
instance_type = "r7g.2xlarge"
pg_version    = 18
extensions    = ["pgvector", "meerkat", "vectorchord"]
name_prefix   = "rekall-r7g"

# ubuntu_version = "24.04"   # 22.04 | 24.04 | 26.04
# huge_pages     = false

[pgvector]
version = "v0.8.2"

[vectorchord]
version = "1.1.1"
```

### Local override (gitignored — `infra/instances/r7g.2xlarge.local.toml`)

Copy `infra/instances/local.toml.example` to start:

```toml
ssh_key              = "~/.ssh/id_ed25519.pub"
iam_instance_profile = "rekall-profile"

[meerkat]
git_url = "git@github.com:myorg/meerkat.git"
git_ref = "main"

[rekall]
git_url = "git@github.com:myorg/rekall.git"
git_ref = "main"
```

### PostgreSQL Auto-Configuration

PG settings are computed automatically from the instance type:

| Setting | Formula | Why |
|---------|---------|-----|
| `shared_buffers` | 25% of RAM | pgvector's own recommendation ("`shared_buffers` should typically be 25% of the server's memory", via PgTune) |
| `effective_cache_size` | 75% of RAM | pgtune |
| `maintenance_work_mem` | 40% of RAM, floored at 256 MB | pgvector builds HNSW far faster when the graph fits here. 40% is also a *ceiling*: the parallel build puts the graph in a DSM segment out of `/dev/shm`, which is 50% of RAM, so going higher reintroduces "could not resize shared memory segment" |
| `work_mem` | 256 MB | |
| `max_wal_size` / `min_wal_size` | 16 GB / 2 GB | fewer, larger checkpoints during bulk load |
| `max_parallel_maintenance_workers` | physical cores, capped at 32 | pgvector uses this directly as its HNSW build worker count; pinned to physical cores because SMT siblings contend on the shared graph |

Computed in `InfraConfig::effective_pg_config`. **If you change a formula,
change this table** — it drifted out of step for months (see the history
below) and a benchmark was misread because of it.

#### Why these values, with sources

**`shared_buffers` = 25%.** This is pgvector's own recommendation, not a
guess and not a compromise:

> Use a tool like PgTune to set initial values for Postgres server
> parameters. For instance, `shared_buffers` should typically be 25% of the
> server's memory.
> — [pgvector README, "Performance"](https://github.com/pgvector/pgvector#performance)

It looks low, and the instinct to raise it recurs. Resist it for benchmark
runs: 25% is what the vendor documents, so results measured under it are
measured under vendor-recommended tuning and cannot be dismissed as a
misconfiguration. Raise it per-run via `[phase_pg_config.search]` if you
want to *test* the sensitivity; do not change the default, which would
silently move every future number and break comparability with published
runs.

Two mechanics worth knowing before arguing about this setting:

- The prewarm caps the buffer-resident slice of a relation at **75% of
  `shared_buffers`** (`PgConnection::prewarm`), reading the remainder into
  the OS page cache. So the pool size bounds how much of an index can be
  pinned where the OS cannot evict it — and a 1953 MB HNSW graph cannot be
  fully pinned on a 4 GiB box at any setting (it would need ~2.6 GB of
  pool, ~70% of usable RAM).
- The usual "shrink `shared_buffers` to avoid double buffering" argument
  assumes the page cache is uncontested. In these benchmarks it is not: the
  cohere-1M heap is ~3.9 GB and the prewarm reads it before the index, so
  page cache is contested and buffer pool is not.

**`maintenance_work_mem` = 40% of RAM, floored at 256 MB.** Both a target
and a ceiling.

Target, because pgvector builds HNSW dramatically faster when the graph
fits:

> Indexes build significantly faster when the graph fits into
> `maintenance_work_mem` … Do not set `maintenance_work_mem` so high that it
> exhausts the memory on the server.
> — [pgvector README, "Index Build Time"](https://github.com/pgvector/pgvector#index-build-time)

Missing that shows up as `NOTICE: hnsw graph no longer fits into
maintenance_work_mem after N tuples`, after which the build falls to an
on-disk pass roughly an order of magnitude slower. **Grep the build log for
that string** before trusting any HNSW build time.

Ceiling, because pgvector's *parallel* build places that graph in a DSM
segment, DSM comes out of `/dev/shm`, and `/dev/shm` is 50% of RAM.
`6bc1c41` capped the formula at 2·RAM/5 after `RAM/2` produced
`could not resize shared memory segment: No space left on device`. Anything
above 40% risks reintroducing it.

Note this affects **builds only**. It has no bearing on search throughput,
so it cannot explain a slow query benchmark.

**`max_parallel_maintenance_workers` = physical cores, capped at 32.**
pgvector uses this value *directly* as its HNSW build worker count:

> `SET max_parallel_maintenance_workers = 7; -- plus leader`
> — [pgvector README, "Index Build Time"](https://github.com/pgvector/pgvector#index-build-time)

Pinned to physical cores (Graviton has no SMT, so vCPU = cores; x86 runs
two threads per core) because the build is CPU- and lock-bound on one shared
graph, so SMT siblings mostly contend. Capped at 32 because scaling is
sublinear past that. `max_parallel_workers` and `max_worker_processes` must
exceed it or the workers never launch.

**`effective_cache_size` = 75%**, **`work_mem` = 256 MB**, **WAL sizes** —
pgtune defaults; the WAL ceiling is raised for fewer, larger checkpoints
during bulk load.

#### Storage

`disk_iops` / `disk_throughput` are optional and **omitted when unset**, so
the volume keeps gp3's included baseline of 3000 IOPS / 125 MiB/s. Omitting
rather than defaulting matters: provisioning above the baseline is billed,
and 16000 IOPS on a 100 GB volume runs ~$108/mo — more than a small
instance costs, enough to dominate a cost comparison.

Every run records the storage it *actually* used (`StorageInfo` in the run
environment, shown as "Instance settings (observed)" on the published run
page): the data directory, its mount point and filesystem, whether the
device is EBS or instance-store NVMe, and the volume's provisioned
IOPS/throughput. Read from the machine, never from the config — see below
for why.

Instance-store NVMe (`pg_on_instance_store`) is wiped by a stop, and
`infra resize` stops the instance, so PGDATA must be on EBS for anything
that walks a ladder of instance sizes.

#### Things that have actually gone wrong here

Recorded so they are not rediscovered:

| Symptom | Cause |
|---|---|
| A benchmark ran at 3000 IOPS with `disk_iops = 16000` in its config | The native launch path built its block-device spec with `VolumeSize`/`VolumeType` only and silently dropped both fields. Fixed; pinned by a test on the spec string, because the failure is invisible at runtime |
| This table said 40% `shared_buffers`, 10% `maintenance_work_mem` | Stale since `eea882e`/`6bc1c41`. Led to arguing from the docs for a value the code never used |
| A 4 GiB box got 1 GB of `maintenance_work_mem`, a 2 GiB box got 50% of RAM | `(ram_gb * 2 / 5).max(1)` — integer division on whole GB, plus a floor that overshot the `/dev/shm` ceiling on small boxes. Now computed in MB |
| A resized box kept the previous rung's volume settings | `infra resize` changed only the instance type. It now applies the target's disk spec via `modify-volume` (online) |
| Storage reported as `device=root, media=unknown` | Ubuntu cloud images name the root filesystem `/dev/root` in `/proc/mounts`, which is not a real device node. Resolved from the directory's `st_dev` via `/sys/dev/block/<major>:<minor>` instead |

The common thread: **a config is a request, not a fact.** Prefer reading the
running machine over trusting what it was asked to be.

Override any setting explicitly in `[pg_config]`:

```toml
[pg_config]
shared_buffers = "32GB"     # override auto-computed value
huge_pages = "try"          # add extra settings
```

### Huge Pages

Set `huge_pages = true` (top-level) to pre-allocate a kernel huge-page pool
sized to `shared_buffers` (with ~10% overhead), disable Transparent Huge
Pages, and set PG `huge_pages = on`. Reduces TLB pressure for random-access
vector index workloads.

```toml
huge_pages = true
```

Note: the reserved huge-page pool is removed from the normal 4KB page
allocator regardless of whether PG uses it. For A/B benchmarks comparing
on vs off, bring up separate instances with this flag set differently —
toggling only the PG-side `huge_pages` GUC on a shared OS configuration
gives a confounded comparison.

### Configuration Reference

| Field | Default | Description |
|-------|---------|-------------|
| `provider` | `"aws"` | Backend: `"aws"` (AWS CLI) or `"terraform"` |
| `region` | `"us-east-1"` | AWS region |
| `instance_type` | `"r7g.2xlarge"` | EC2 instance type |
| `disk_size_gb` | `100` | Root EBS volume size (gp3) |
| `disk_iops` | (default) | gp3 IOPS (default 3000) |
| `disk_throughput` | (default) | gp3 throughput MiB/s (default 125) |
| `pg_version` | `18` | PostgreSQL major version |
| `pg_port` | `5432` | PostgreSQL listen port |
| `ubuntu_version` | `"24.04"` | Ubuntu LTS version (`22.04`, `24.04`, or `26.04`) |
| `extensions` | `[]` | Extensions to install |
| `huge_pages` | `false` | Reserve kernel huge pages + disable THP + set PG `huge_pages = on` |
| `name_prefix` | `"rekall"` | Resource naming prefix |
| `ssh_key` | `~/.ssh/id_ed25519.pub` | Path to your SSH public key |
| `iam_instance_profile` | (required) | Pre-created IAM instance profile name |
| `s3_bucket` | (all) | S3 bucket for IAM policy scoping |

## SSH Keys

Rekall uses three separate SSH key sources:

### 1. Launcher key (`ssh_key`)

Your personal SSH key for logging into instances you launched. Only the
**public key** is uploaded to AWS (as an EC2 key pair). The private key
never leaves your machine.

```toml
# In .local.toml (never committed)
ssh_key = "~/.ssh/id_ed25519.pub"
```

### 2. Team authorized_keys (`infra/authorized_keys`)

A committed file of permanent public keys that get installed on **every**
rekall instance at boot, regardless of who launched it. This is how
teammates SSH into each other's instances — useful for inspecting a
benchmark run on a colleague's machine, or attaching to a remote tmux
session.

Add one OpenSSH public key per line, with a trailing comment for
attribution. Adding a key here grants permanent access to all future
rekall instances under any team member's launch — only add keys you
trust.

The launcher's `ssh_key` is also injected into `authorized_keys`, but
deduped if its body already appears in the committed file.

### 3. Deploy keys (auto-generated, per-repo)

Key pairs for instances to access private GitHub repos. Rekall generates
**one per repo, shared across every instance you ever launch**, stored in
`.rekall/infra/deploy-keys/`. Register each public key on the corresponding
GitHub repo **once** — the same key is reused for every `name_prefix` and
every instance type going forward.

The private keys are uploaded to instances via SCP over SSH (encrypted,
never in provider state or instance metadata). Each `git clone` uses
`GIT_SSH_COMMAND` to select the correct key.

Keys persist across `infra up`/`down` cycles. Regenerate with
`rekall infra deploy-key --regenerate` if you need to rotate them (you'll
need to update the public key on GitHub afterward).

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

## Backends

Rekall supports two backends for provisioning EC2 instances. Both
produce the same result; they differ in dependencies and state
management.

### AWS CLI backend (default)

```toml
[infra]
provider = "aws"
```

Uses the `aws` CLI directly (`aws ec2 run-instances`, etc.). No
Terraform needed. State is a simple JSON file at
`.rekall/infra/{name_prefix}/aws-state.json` tracking the instance ID,
security group, and key pair.

**Requires:** AWS CLI (`aws`) on PATH.

**Teardown:** `rekall infra down` terminates the instance, deletes the
security group and key pair, and removes the state file. If the state
file is lost, terminate the instance manually in the AWS console.

### Terraform backend

```toml
[infra]
provider = "terraform"
```

Uses Terraform for provisioning. HCL files are embedded in the rekall
binary and written to `.rekall/infra/{name_prefix}/terraform/` at
runtime. Terraform manages its own state file (`terraform.tfstate`).

**Requires:** Terraform (>= 1.5) on PATH.

**Advantages over AWS CLI backend:**
- Idempotent — `infra up` is safe to run repeatedly
- Richer state tracking — handles partial failures better
- SSH provisioners built into Terraform's apply step

**Teardown:** `rekall infra down` runs `terraform destroy`. If the state
is lost, terminate the instance manually in the AWS console.

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

**AWS CLI not found**: Install from
https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html

**Terraform not found** (terraform backend only): Install from
https://developer.hashicorp.com/terraform/install

**AWS credentials not configured**: Run `aws configure` or set
`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`.

**IAM instance profile missing**: Run `rekall infra init` and follow the
instructions to apply the IAM template.

**SSH key not found**: Default is `~/.ssh/id_ed25519.pub`. Set `ssh_key` in `~/.config/rekall/infra.toml` to point at the
path of your SSH public key (e.g., `~/.ssh/id_ed25519.pub`).

**Deploy key not generated**: Run `rekall infra init` or
`rekall infra deploy-key`.

**Provisioning timeout**: The instance started but PostgreSQL didn't
become ready within the timeout. SSH in manually and check
`/var/log/rekall-setup.log`:

```bash
ssh -i ~/.ssh/id_ed25519 ubuntu@<ip>
sudo cat /var/log/rekall-setup.log
```

**Private repo clone failed**: Verify each repo's deploy key is added
to the correct GitHub repo. Each repo needs its own unique deploy key
(GitHub doesn't allow sharing keys across repos).

**S3 access denied**: Check that the IAM role has the correct S3
permissions and the bucket policy allows the role (for cross-account).

**Security group already exists** (aws backend): A previous `infra down`
may have failed partially. Delete the security group manually:
`aws ec2 delete-security-group --group-name rekall-sg`
