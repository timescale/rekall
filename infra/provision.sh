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
    MEERKAT_SSH_CMD="ssh -i $HOME/.ssh/meerkat -o StrictHostKeyChecking=no"

    if [ -f "$MEERKAT_SO" ]; then
        echo "=== Meerkat already installed ($MEERKAT_SO exists), skipping ==="
    else
        echo "=== Building meerkat from source ==="

        # The initial clone needs GIT_SSH_COMMAND because .git/config doesn't
        # exist yet. Every subsequent git operation in this repo uses the
        # core.sshCommand we set below, so users SSH'ing in to pull/push
        # later don't need to export anything.
        if [ ! -d "$MEERKAT_DIR" ]; then
            GIT_SSH_COMMAND="$MEERKAT_SSH_CMD" git clone "$MEERKAT_GIT_URL" "$MEERKAT_DIR"
        fi
        cd "$MEERKAT_DIR"
        git config core.sshCommand "$MEERKAT_SSH_CMD"

        git fetch origin
        git checkout "$MEERKAT_GIT_REF"
        git pull origin "$MEERKAT_GIT_REF" || true

        # Parallelise LTO across all available cores. Without this the link
        # phase is single-threaded and pegs one core for many minutes.
        LTO_THREADS=$(nproc 2>/dev/null || echo 1)
        MEERKAT_MESON_OPTS="--buildtype=release -Dnative=true -Db_lto=true -Db_lto_threads=$LTO_THREADS"
        case "$MEERKAT_MESON_OPTS" in
            *"-Db_lto=true"*)
                echo "NOTE: LTO is enabled with $LTO_THREADS parallel threads."
                echo "      The link step still runs silently for a few minutes;"
                echo "      this is expected, not a hang."
                ;;
        esac

        sudo bash -c "
            cd $MEERKAT_DIR
            rm -rf builddir
            meson setup builddir $MEERKAT_MESON_OPTS -Dpg_config=$PG_CONFIG
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

        # pgrx version must match VectorChord's Cargo.toml. Handle:
        #   pgrx = "0.x.y"
        #   pgrx = { version = "0.x.y", ... }
        #   [dependencies.pgrx] then version = "0.x.y" (table form)
        # by finding the first quoted version-shaped token on or after a
        # pgrx-related line. Disable -e around the parse so a no-match
        # doesn't silently kill the script via pipefail.
        set +e
        PGRX_VERSION=$(awk '
            /^\[/ { in_pgrx=0 }
            /^[[:space:]]*pgrx[[:space:]]*=/ { in_pgrx=1 }
            /^\[(workspace\.)?dependencies\.pgrx\]/ { in_pgrx=1 }
            in_pgrx && match($0, /"[=^~]?[0-9][^"]*"/) {
                v = substr($0, RSTART+1, RLENGTH-2)
                sub(/^[=^~]/, "", v)
                print v
                exit
            }
        ' Cargo.toml)
        set -e

        if [ -z "$PGRX_VERSION" ]; then
            echo "ERROR: could not parse pgrx version from Cargo.toml" >&2
            echo "  pwd: $(pwd)" >&2
            echo "  pgrx-related lines:" >&2
            grep -n -i pgrx Cargo.toml >&2 || echo "  (none found)" >&2
            exit 1
        fi

        echo "Installing cargo-pgrx ${PGRX_VERSION}..."
        cargo install "cargo-pgrx@${PGRX_VERSION}" --locked
        cargo pgrx init "--pg${PG_VERSION}=$PG_CONFIG"

        # --sudo: build as ubuntu (keeps target/ cache here) but use sudo
        # for the final copy into /usr/{lib,share}/postgresql/*/.
        cargo pgrx install --release --sudo --pg-config="$PG_CONFIG"
        sudo systemctl restart "postgresql@${PG_VERSION}-main"
        echo "=== VectorChord installed ==="
    fi

    # VectorChord needs its library preloaded. Nothing else sets
    # shared_preload_libraries (meerkat is a plain IAM and needs no preload), so
    # add vchord here -- after the .so is installed -- and restart. This must
    # live in provision.sh, not the config's [pg_config]: cloud-init starts PG
    # before this script runs, so vchord.so only exists now; setting the preload
    # earlier would make PG fail to start. Runs for both the freshly-built and
    # already-installed paths; idempotent.
    PG_CONF="/etc/postgresql/${PG_VERSION}/main/postgresql.conf"
    if ! grep -qE "^[[:space:]]*shared_preload_libraries[[:space:]]*=.*vchord" \
        "$PG_CONF"; then
        echo "shared_preload_libraries = 'vchord'" | sudo tee -a "$PG_CONF" >/dev/null
        sudo systemctl restart "postgresql@${PG_VERSION}-main"
        echo "=== Enabled vchord in shared_preload_libraries ==="
    fi
fi

# --- Rekall ---

if [ -n "$REKALL_GIT_URL" ]; then
    REKALL_DIR="$REPOS_DIR/rekall"

    REKALL_SSH_CMD="ssh -i $HOME/.ssh/rekall -o StrictHostKeyChecking=no"

    if command -v rekall &>/dev/null; then
        echo "=== Rekall already installed ($(rekall --version)), skipping ==="
    else
        echo "=== Building rekall from source ==="
        source "$HOME/.cargo/env"

        # See the meerkat block: GIT_SSH_COMMAND only for the initial
        # clone, then persist via core.sshCommand for the user's later
        # pulls/pushes.
        if [ ! -d "$REKALL_DIR" ]; then
            GIT_SSH_COMMAND="$REKALL_SSH_CMD" git clone "$REKALL_GIT_URL" "$REKALL_DIR"
        fi
        cd "$REKALL_DIR"
        git config core.sshCommand "$REKALL_SSH_CMD"

        git fetch origin
        git checkout "$REKALL_GIT_REF"
        git pull origin "$REKALL_GIT_REF" || true

        cargo build --release
        sudo cp "$REKALL_DIR/target/release/rekall" /usr/local/bin/
        echo "=== Rekall installed ==="
    fi

    # Generate global config with cloud-appropriate paths
    PG_PORT="${PG_PORT:-5432}"
    BENCH_DIR="$HOME/benchmarks"
    rekall setup --force \
        --data-dir "$BENCH_DIR/data" \
        --results-dir "$BENCH_DIR/results" \
        --dsn "host=localhost port=$PG_PORT dbname=rekall user=rekall password=rekall"
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
