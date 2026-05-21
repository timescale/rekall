#!/bin/bash
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

# PostgreSQL PGDG repository
/usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y
apt-get install -y postgresql-${pg_version} postgresql-server-dev-${pg_version}

# pgvector
%{ if contains(extensions, "pgvector") }
echo "=== Installing pgvector ==="
apt-get install -y postgresql-${pg_version}-pgvector || {
  echo "pgvector apt package not available, building from source"
  cd /tmp
  git clone --depth 1 https://github.com/pgvector/pgvector.git
  cd pgvector
  make PG_CONFIG=/usr/lib/postgresql/${pg_version}/bin/pg_config
  make PG_CONFIG=/usr/lib/postgresql/${pg_version}/bin/pg_config install
}
%{ endif }

# Configure PostgreSQL
PG_CONF="/etc/postgresql/${pg_version}/main/postgresql.conf"
PG_HBA="/etc/postgresql/${pg_version}/main/pg_hba.conf"

sed -i "s/#listen_addresses = 'localhost'/listen_addresses = '*'/" "$PG_CONF"
sed -i "s/^port = .*/port = ${pg_port}/" "$PG_CONF"

%{ for key, value in pg_config }
echo "${key} = '${value}'" >> "$PG_CONF"
%{ endfor }

# Allow remote connections for rekall user
echo "host all rekall 0.0.0.0/0 scram-sha-256" >> "$PG_HBA"

# Restart and create user + database
systemctl restart postgresql@${pg_version}-main

sudo -u postgres psql -p ${pg_port} -c "CREATE USER rekall WITH PASSWORD 'rekall' SUPERUSER"
sudo -u postgres psql -p ${pg_port} -c "CREATE DATABASE rekall OWNER rekall"

# Rust toolchain (installed for ubuntu user)
echo "=== Installing Rust ==="
sudo -u ubuntu bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'

# Signal readiness
touch /tmp/rekall-ready

echo "=== Rekall instance setup complete ==="
date
