variable "region" {
  description = "AWS region"
  type        = string
  default     = "us-east-1"
}

variable "instance_type" {
  description = "EC2 instance type (Graviton ARM by default)"
  type        = string
  default     = "r7g.2xlarge"
}

variable "disk_size_gb" {
  description = "Root EBS volume size in GB"
  type        = number
  default     = 1000
}

variable "disk_iops" {
  description = "gp3 IOPS (0 = default 3000)"
  type        = number
  default     = 0
}

variable "disk_throughput" {
  description = "gp3 throughput in MiB/s (0 = default 125)"
  type        = number
  default     = 0
}

variable "pg_version" {
  description = "PostgreSQL major version"
  type        = number
  default     = 18
}

variable "ubuntu_version" {
  description = "Ubuntu LTS version (e.g., 24.04, 26.04)"
  type        = string
  default     = "24.04"
}

variable "ubuntu_codename" {
  description = "Ubuntu codename matching ubuntu_version (e.g., noble, resolute)"
  type        = string
  default     = "noble"
}

variable "ssh_public_key" {
  description = "SSH public key content"
  type        = string
}

variable "ssh_cidr" {
  description = "CIDR block for SSH access (e.g. 1.2.3.4/32)"
  type        = string
  default     = "0.0.0.0/0"
}

variable "name_prefix" {
  description = "Resource naming prefix"
  type        = string
  default     = "rekall"
}

variable "rekall_git_url" {
  description = "Git URL for rekall (empty to skip)"
  type        = string
  default     = ""
}

variable "rekall_git_ref" {
  description = "Git ref for rekall build"
  type        = string
  default     = "main"
}

variable "meerkat_git_url" {
  description = "Git URL for meerkat source build (empty to skip)"
  type        = string
  default     = ""
}

variable "meerkat_git_ref" {
  description = "Git ref for meerkat build"
  type        = string
  default     = "main"
}

variable "iam_instance_profile" {
  description = "Pre-created IAM instance profile name (from `rekall infra iam-policy`)"
  type        = string
  default     = "rekall-profile"
}

variable "pg_port" {
  description = "PostgreSQL listen port"
  type        = number
  default     = 5432
}

variable "ssh_private_key_path" {
  description = "Path to SSH private key for connecting to the instance"
  type        = string
}

variable "github_ssh_key_paths" {
  description = "Paths to deploy key private keys for GitHub repos"
  type        = list(string)
  default     = []
}

variable "threads_per_core" {
  description = "CPU threads per core (1 disables hyperthreading/SMT); null keeps the instance default"
  type        = number
  default     = null
}
