#!/bin/bash
# Post-launch provisioning script for rekall benchmark instances.
#
# This script is uploaded to the instance and run via SSH after
# cloud-init completes. It is idempotent — safe to run multiple
# times. Each step checks whether its work is already done.
#
# Environment variables (set by rekall before running):
#   PG_VERSION         — PostgreSQL major version (e.g., 18)
#   MEERKAT_GIT_URL    — git URL for meerkat (empty to skip)
#   MEERKAT_GIT_REF    — git ref for meerkat (default: main)
#   REKALL_GIT_URL     — git URL for rekall (empty to skip)
#   REKALL_GIT_REF     — git ref for rekall (default: main)

set -euo pipefail

PG_VERSION="${PG_VERSION:-18}"
MEERKAT_GIT_URL="${MEERKAT_GIT_URL:-}"
MEERKAT_GIT_REF="${MEERKAT_GIT_REF:-main}"
REKALL_GIT_URL="${REKALL_GIT_URL:-}"
REKALL_GIT_REF="${REKALL_GIT_REF:-main}"

PG_CONFIG="/usr/lib/postgresql/${PG_VERSION}/bin/pg_config"

# --- Meerkat ---

if [ -n "$MEERKAT_GIT_URL" ]; then
    MEERKAT_SO="/usr/lib/postgresql/${PG_VERSION}/lib/meerkat.so"
    if [ -f "$MEERKAT_SO" ]; then
        echo "=== Meerkat already installed ($MEERKAT_SO exists), skipping ==="
    else
        echo "=== Building meerkat from source ==="
        sudo bash -c "
            export GIT_SSH_COMMAND='ssh -i /root/.ssh/meerkat -o StrictHostKeyChecking=no'
            rm -rf /tmp/meerkat
            cd /tmp
            git clone $MEERKAT_GIT_URL meerkat
            cd meerkat
            git checkout $MEERKAT_GIT_REF
            meson setup builddir --buildtype=release -Dpg_config=$PG_CONFIG
            meson compile -C builddir
            meson install -C builddir
        "
        sudo systemctl restart "postgresql@${PG_VERSION}-main"
        echo "=== Meerkat installed ==="
    fi
fi

# --- Rekall ---

if [ -n "$REKALL_GIT_URL" ]; then
    if command -v rekall &>/dev/null; then
        echo "=== Rekall already installed ($(rekall --version)), skipping ==="
    else
        echo "=== Building rekall from source ==="
        export GIT_SSH_COMMAND="ssh -i $HOME/.ssh/rekall -o StrictHostKeyChecking=no"
        source "$HOME/.cargo/env"
        rm -rf /tmp/rekall
        cd /tmp
        git clone "$REKALL_GIT_URL" rekall
        cd rekall
        git checkout "$REKALL_GIT_REF"
        cargo build --release
        sudo cp /tmp/rekall/target/release/rekall /usr/local/bin/
        echo "=== Rekall installed ==="
    fi
fi

echo "=== Provisioning complete ==="
