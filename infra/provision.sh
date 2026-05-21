#!/bin/bash
# Post-launch provisioning script for rekall benchmark instances.
#
# This script is uploaded to the instance and run via SSH after
# cloud-init completes. It is idempotent — safe to run multiple
# times. Each step checks whether its work is already done.
#
# Repos are cloned to ~/repos/ so the user can later update code
# with `cd ~/repos/meerkat && git pull && ...` without reprovisioning.
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
REPOS_DIR="$HOME/repos"
mkdir -p "$REPOS_DIR"

# --- Meerkat ---

if [ -n "$MEERKAT_GIT_URL" ]; then
    MEERKAT_SO="/usr/lib/postgresql/${PG_VERSION}/lib/meerkat.so"
    MEERKAT_DIR="$REPOS_DIR/meerkat"

    if [ -f "$MEERKAT_SO" ]; then
        echo "=== Meerkat already installed ($MEERKAT_SO exists), skipping ==="
    else
        echo "=== Building meerkat from source ==="
        export GIT_SSH_COMMAND="ssh -i $HOME/.ssh/meerkat -o StrictHostKeyChecking=no"

        if [ -d "$MEERKAT_DIR" ]; then
            cd "$MEERKAT_DIR"
            git fetch origin
            git checkout "$MEERKAT_GIT_REF"
            git pull origin "$MEERKAT_GIT_REF" || true
        else
            git clone "$MEERKAT_GIT_URL" "$MEERKAT_DIR"
            cd "$MEERKAT_DIR"
            git checkout "$MEERKAT_GIT_REF"
        fi

        sudo bash -c "
            cd $MEERKAT_DIR
            rm -rf builddir
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
    REKALL_DIR="$REPOS_DIR/rekall"

    if command -v rekall &>/dev/null; then
        echo "=== Rekall already installed ($(rekall --version)), skipping ==="
    else
        echo "=== Building rekall from source ==="
        export GIT_SSH_COMMAND="ssh -i $HOME/.ssh/rekall -o StrictHostKeyChecking=no"
        source "$HOME/.cargo/env"

        if [ -d "$REKALL_DIR" ]; then
            cd "$REKALL_DIR"
            git fetch origin
            git checkout "$REKALL_GIT_REF"
            git pull origin "$REKALL_GIT_REF" || true
        else
            git clone "$REKALL_GIT_URL" "$REKALL_DIR"
            cd "$REKALL_DIR"
            git checkout "$REKALL_GIT_REF"
        fi

        cargo build --release
        sudo cp "$REKALL_DIR/target/release/rekall" /usr/local/bin/
        echo "=== Rekall installed ==="
    fi
fi

echo "=== Provisioning complete ==="
echo ""
echo "Repos cloned to ~/repos/. To update later:"
if [ -n "$MEERKAT_GIT_URL" ]; then
    echo "  cd ~/repos/meerkat && git pull && sudo meson compile -C builddir && sudo meson install -C builddir"
fi
if [ -n "$REKALL_GIT_URL" ]; then
    echo "  cd ~/repos/rekall && git pull && source ~/.cargo/env && cargo build --release && sudo cp target/release/rekall /usr/local/bin/"
fi
