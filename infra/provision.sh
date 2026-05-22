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
#   VECTORCHORD        — set to "1" to install VectorChord

set -euo pipefail

PG_VERSION="${PG_VERSION:-18}"
MEERKAT_GIT_URL="${MEERKAT_GIT_URL:-}"
MEERKAT_GIT_REF="${MEERKAT_GIT_REF:-main}"
REKALL_GIT_URL="${REKALL_GIT_URL:-}"
REKALL_GIT_REF="${REKALL_GIT_REF:-main}"
VECTORCHORD="${VECTORCHORD:-}"
VECTORCHORD_VERSION="${VECTORCHORD_VERSION:-1.1.1}"

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
            meson setup builddir --buildtype=release -Dnative=true -Db_lto=true -Dpg_config=$PG_CONFIG
            meson compile -C builddir
            meson install -C builddir
        "
        sudo systemctl restart "postgresql@${PG_VERSION}-main"
        echo "=== Meerkat installed ==="
    fi
fi

# --- VectorChord ---

if [ "$VECTORCHORD" = "1" ]; then
    VCHORD_SO="/usr/lib/postgresql/${PG_VERSION}/lib/vchord.so"
    VCHORD_DIR="$REPOS_DIR/vectorchord"

    if [ -f "$VCHORD_SO" ]; then
        echo "=== VectorChord already installed ($VCHORD_SO exists), skipping ==="
    else
        echo "=== Building VectorChord from source ==="
        source "$HOME/.cargo/env"

        if [ -d "$VCHORD_DIR" ]; then
            cd "$VCHORD_DIR"
            git fetch origin --tags
        else
            git clone https://github.com/tensorchord/VectorChord.git "$VCHORD_DIR"
            cd "$VCHORD_DIR"
        fi
        git checkout "$VECTORCHORD_VERSION"

        # pgrx version must match VectorChord's Cargo.toml. Handle both
        # `pgrx = "0.x.y"` (bare) and `pgrx = { version = "0.x.y", ... }`
        # (table) by picking the first quoted string starting with a digit.
        PGRX_VERSION=$(grep '^pgrx ' Cargo.toml | head -1 \
            | grep -oE '"[0-9][^"]*"' | head -1 | tr -d '"')
        if [ -z "$PGRX_VERSION" ]; then
            echo "ERROR: could not parse pgrx version from Cargo.toml:" >&2
            grep '^pgrx ' Cargo.toml >&2
            exit 1
        fi

        echo "Installing cargo-pgrx ${PGRX_VERSION}..."
        cargo install "cargo-pgrx@${PGRX_VERSION}" --locked
        cargo pgrx init "--pg${PG_VERSION}=$PG_CONFIG"

        cargo pgrx install --release --pg-config="$PG_CONFIG"
        sudo systemctl restart "postgresql@${PG_VERSION}-main"
        echo "=== VectorChord installed ==="
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
if [ "$VECTORCHORD" = "1" ]; then
    echo "  cd ~/repos/vectorchord && git pull && cargo pgrx install --release --pg-config=$PG_CONFIG"
fi
if [ -n "$REKALL_GIT_URL" ]; then
    echo "  cd ~/repos/rekall && git pull && source ~/.cargo/env && cargo build --release && sudo cp target/release/rekall /usr/local/bin/"
fi
