variable "arch" {
  description = "CPU architecture: arm64 (Graviton) or x86_64"
  type        = string
  default     = "arm64"
}

locals {
  ami_arch = var.arch == "arm64" ? "arm64" : "amd64"
}

data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-${local.ami_arch}-server-*"]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }

  filter {
    name   = "architecture"
    values = [var.arch]
  }
}

resource "aws_key_pair" "rekall" {
  key_name   = "${var.name_prefix}-key"
  public_key = var.ssh_public_key
}

resource "aws_security_group" "rekall" {
  name        = "${var.name_prefix}-sg"
  description = "Rekall benchmark instance"

  ingress {
    description = "SSH"
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = [var.ssh_cidr]
  }

  ingress {
    description = "PostgreSQL"
    from_port   = var.pg_port
    to_port     = var.pg_port
    protocol    = "tcp"
    cidr_blocks = [var.ssh_cidr]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = {
    Name = "${var.name_prefix}-sg"
  }
}

resource "aws_instance" "benchmark" {
  ami                  = data.aws_ami.ubuntu.id
  instance_type        = var.instance_type
  key_name             = aws_key_pair.rekall.key_name
  iam_instance_profile = var.iam_instance_profile

  vpc_security_group_ids = [aws_security_group.rekall.id]

  root_block_device {
    volume_size = var.disk_size_gb
    volume_type = "gp3"
    iops        = var.disk_iops > 0 ? var.disk_iops : null
    throughput  = var.disk_throughput > 0 ? var.disk_throughput : null
  }

  user_data = templatefile("${path.module}/user-data.sh.tpl", {
    pg_version   = var.pg_version
    extensions   = var.extensions
    pg_config    = var.pg_config
    pg_port      = var.pg_port
    huge_pages   = var.huge_pages
    nr_hugepages = var.nr_hugepages
  })

  connection {
    type        = "ssh"
    user        = "ubuntu"
    private_key = file(var.ssh_private_key_path)
    host        = self.public_ip
    timeout     = "10m"
  }

  # Wait for cloud-init to finish before running provisioners
  provisioner "remote-exec" {
    inline = [
      "echo 'Waiting for cloud-init...'",
      "cloud-init status --wait",
      "echo 'cloud-init complete'",
    ]
  }

  # Upload deploy keys directory (if any keys exist)
  provisioner "file" {
    source      = length(var.github_ssh_key_paths) > 0 ? dirname(var.github_ssh_key_paths[0]) : "/dev/null"
    destination = "/tmp/deploy-keys"
  }

  # Install deploy keys + build private repos
  provisioner "remote-exec" {
    inline = concat(
      # Install per-repo deploy keys for both root and ubuntu
      length(var.github_ssh_key_paths) > 0 ? [
        "sudo bash -c 'install -m 700 -d /root/.ssh && ssh-keyscan github.com >> /root/.ssh/known_hosts 2>/dev/null'",
        "mkdir -p ~/.ssh && ssh-keyscan github.com >> ~/.ssh/known_hosts 2>/dev/null",
        "for key in /tmp/deploy-keys/*; do [ -f \"$key\" ] && [[ \"$key\" != *.pub ]] && name=$(basename \"$key\") && sudo cp \"$key\" \"/root/.ssh/$name\" && sudo chmod 600 \"/root/.ssh/$name\" && cp \"$key\" \"$HOME/.ssh/$name\" && chmod 600 \"$HOME/.ssh/$name\"; done",
        "rm -rf /tmp/deploy-keys",
      ] : [],

      # Build meerkat from source (uses per-repo deploy key)
      var.meerkat_git_url != "" ? [
        "sudo bash -c 'export GIT_SSH_COMMAND=\"ssh -i /root/.ssh/meerkat -o StrictHostKeyChecking=no\" && cd /tmp && git clone ${var.meerkat_git_url} meerkat && cd meerkat && git checkout ${var.meerkat_git_ref} && meson setup builddir --buildtype=release -Dpg_config=/usr/lib/postgresql/${var.pg_version}/bin/pg_config && meson compile -C builddir && meson install -C builddir'",
        "sudo systemctl restart postgresql@${var.pg_version}-main",
      ] : [],

      # Build rekall from source (uses per-repo deploy key)
      var.rekall_git_url != "" ? [
        "bash -c 'export GIT_SSH_COMMAND=\"ssh -i ~/.ssh/rekall -o StrictHostKeyChecking=no\" && source ~/.cargo/env && cd /tmp && git clone ${var.rekall_git_url} rekall && cd rekall && git checkout ${var.rekall_git_ref} && cargo build --release'",
        "sudo cp /tmp/rekall/target/release/rekall /usr/local/bin/",
      ] : [],

      ["echo 'Provisioning complete'"],
    )
  }

  tags = {
    Name = "${var.name_prefix}-benchmark"
  }
}
