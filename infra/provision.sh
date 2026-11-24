#!/bin/bash
# Post-launch provisioning script for rekall benchmark instances.
#
# This script is uploaded to the instance and run via SSH after
# cloud-init completes. It is idempotent — safe to run multiple
# times. Each step checks whether its work is already done.
#
# Repos are cloned to ~/repos/ so the user can later update code
# with `cd ~/repos/pg_vectorsearch && git pull && ...` without reprovisioning.
#
# Environment variables (set by rekall before running):
#   PG_VERSION         — PostgreSQL major version (e.g., 18)
#   PG_PORT            — PostgreSQL port (default: 5432)
#   PG_VECTORSEARCH_GIT_URL    — git URL for pg_vectorsearch (empty to skip)
#   PG_VECTORSEARCH_GIT_REF    — git ref for pg_vectorsearch (default: main)
#   REKALL_GIT_URL     — git URL for rekall (empty to skip)
#   REKALL_GIT_REF     — git ref for rekall (default: main)
#   VECTORCHORD        — set to "1" to install VectorChord
#   PARADEDB           — set to "1" to install ParadeDB's pg_search
#   PARADEDB_VERSION   — pg_search release (default: 0.27.0)

set -euo pipefail

PG_VERSION="${PG_VERSION:-18}"
PG_VECTORSEARCH_GIT_URL="${PG_VECTORSEARCH_GIT_URL:-}"
PG_VECTORSEARCH_GIT_REF="${PG_VECTORSEARCH_GIT_REF:-main}"
REKALL_GIT_URL="${REKALL_GIT_URL:-}"
REKALL_GIT_REF="${REKALL_GIT_REF:-main}"
VECTORCHORD="${VECTORCHORD:-}"
VECTORCHORD_VERSION="${VECTORCHORD_VERSION:-1.1.1}"
PARADEDB="${PARADEDB:-}"
PARADEDB_VERSION="${PARADEDB_VERSION:-0.27.0}"
PG_PORT="${PG_PORT:-5432}"

PG_CONFIG="/usr/lib/postgresql/${PG_VERSION}/bin/pg_config"
PG_UNIT="postgresql@${PG_VERSION}-main"
REPOS_DIR="$HOME/repos"
mkdir -p "$REPOS_DIR"

pg_admin() {
    sudo -u postgres psql -X -p "$PG_PORT" -d postgres -v ON_ERROR_STOP=1 "$@"
}

wait_for_postgres() {
    for _ in $(seq 1 60); do
        if pg_admin -c 'SELECT 1' >/dev/null 2>&1; then
            return
        fi
        sleep 1
    done
    echo "ERROR: PostgreSQL did not accept connections on port $PG_PORT" >&2
    return 1
}

has_preload_library() {
    local normalized="${1//[[:space:]\"]/}"
    [[ ",$normalized," == *",$2,"* || ",$normalized," == *",\$libdir/$2,"* ]]
}

package_dir=
package_path=
auto_conf=
auto_backup=
recover_postgres=0
cleanup() {
    local status=$?
    if (( status != 0 && recover_postgres )); then
        if [ -n "$auto_backup" ]; then
            sudo mv -f -- "$auto_backup" "$auto_conf" ||
                echo "ERROR: Could not restore $auto_conf from $auto_backup" >&2
            sudo systemctl restart "$PG_UNIT" ||
                echo "ERROR: Could not restart $PG_UNIT with the prior configuration" >&2
        else
            sudo systemctl start "$PG_UNIT" ||
                echo "ERROR: Could not start $PG_UNIT after provisioning failed" >&2
        fi
    fi
    if [ -n "$package_dir" ]; then
        rm -f -- "$package_path" && rmdir -- "$package_dir" ||
            echo "WARNING: Could not clean up $package_dir" >&2
    fi
    return "$status"
}
trap cleanup EXIT

# Resolve and fetch the package before taking PostgreSQL offline.
if [ "$PARADEDB" = "1" ]; then
    CODENAME=$(. /etc/os-release && echo "${VERSION_CODENAME:-}")
    ARCH=$(dpkg --print-architecture)
    if [[ ! "$PARADEDB_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ||
          ! "$PG_VERSION" =~ ^(15|16|17|18)$ ||
          ! "$CODENAME" =~ ^(noble|resolute)$ ||
          ! "$ARCH" =~ ^(amd64|arm64)$ ]]; then
        echo "ERROR: Unsupported pg_search package: $PARADEDB_VERSION, PostgreSQL $PG_VERSION, $CODENAME/$ARCH" >&2
        exit 1
    fi
    PG_SEARCH_PKG="postgresql-${PG_VERSION}-pg-search"
    INSTALLED=$(dpkg-query -W -f='${db:Status-Status} ${Version}' "$PG_SEARCH_PKG" 2>/dev/null || true)
    if [[ "$INSTALLED" == "installed $PARADEDB_VERSION" ||
          "$INSTALLED" == "installed ${PARADEDB_VERSION}-"* ]]; then
        echo "=== pg_search $PARADEDB_VERSION already installed, skipping ==="
    elif [[ "$INSTALLED" == installed\ * ]] &&
         dpkg --compare-versions "${INSTALLED#installed }" gt "$PARADEDB_VERSION"; then
        echo "ERROR: Refusing to downgrade pg_search from ${INSTALLED#installed } to $PARADEDB_VERSION" >&2
        exit 1
    else
        DEB="${PG_SEARCH_PKG}_${PARADEDB_VERSION}-1PARADEDB-${CODENAME}_${ARCH}.deb"
        package_dir=$(mktemp -d)
        package_path="$package_dir/$DEB"
        curl -fsSL --retry 3 -o "$package_path" \
            "https://github.com/paradedb/paradedb/releases/download/v${PARADEDB_VERSION}/$DEB"
    fi
fi

# Stop PostgreSQL for the duration of the source builds below and start it
# again as the last step (see the end of this script). The builds are
# memory-heavy -- a release rustc for aws-sdk-s3 alone peaks over 1 GB -- and
# on a small box (e.g. the 2 GiB c8g.medium) the running server's
# shared_buffers plus the compiler exceed RAM, so the OOM killer takes rustc
# mid-build. Nothing here needs the server: the rekall role and database are
# created earlier in cloud-init, `cargo pgrx install` and `meson install` only
# copy files (no live server), and `rekall setup` just writes a config file.
echo "=== Stopping PostgreSQL to free memory during source builds ==="
recover_postgres=1
sudo systemctl stop "$PG_UNIT"

# --- PRISM ---

if [ -n "$PG_VECTORSEARCH_GIT_URL" ]; then
    PG_VECTORSEARCH_CONTROL="/usr/share/postgresql/${PG_VERSION}/extension/pg_vectorsearch.control"
    PG_VECTORSEARCH_DIR="$REPOS_DIR/pg_vectorsearch"
    PG_VECTORSEARCH_SSH_CMD="ssh -i $HOME/.ssh/pg_vectorsearch -o StrictHostKeyChecking=no"

    if [ -f "$PG_VECTORSEARCH_CONTROL" ]; then
        echo "=== pg_vectorsearch already installed ($PG_VECTORSEARCH_CONTROL exists), skipping ==="
    else
        echo "=== Building pg_vectorsearch from source ==="

        # The initial clone needs GIT_SSH_COMMAND because .git/config doesn't
        # exist yet. Every subsequent git operation in this repo uses the
        # core.sshCommand we set below, so users SSH'ing in to pull/push
        # later don't need to export anything.
        if [ ! -d "$PG_VECTORSEARCH_DIR" ]; then
            GIT_SSH_COMMAND="$PG_VECTORSEARCH_SSH_CMD" git clone "$PG_VECTORSEARCH_GIT_URL" "$PG_VECTORSEARCH_DIR"
        fi
        cd "$PG_VECTORSEARCH_DIR"
        git config core.sshCommand "$PG_VECTORSEARCH_SSH_CMD"

        git fetch origin
        git checkout "$PG_VECTORSEARCH_GIT_REF"
        git pull origin "$PG_VECTORSEARCH_GIT_REF" || true

        # Parallelise LTO across all available cores. Without this the link
        # phase is single-threaded and pegs one core for many minutes.
        LTO_THREADS=$(nproc 2>/dev/null || echo 1)
        PG_VECTORSEARCH_MESON_OPTS="--buildtype=release -Dnative=true -Db_lto=true -Db_lto_threads=$LTO_THREADS"
        case "$PG_VECTORSEARCH_MESON_OPTS" in
            *"-Db_lto=true"*)
                echo "NOTE: LTO is enabled with $LTO_THREADS parallel threads."
                echo "      The link step still runs silently for a few minutes;"
                echo "      this is expected, not a hang."
                ;;
        esac

        sudo bash -c "
            cd $PG_VECTORSEARCH_DIR
            rm -rf builddir
            meson setup builddir $PG_VECTORSEARCH_MESON_OPTS -Dpg_config=$PG_CONFIG
            meson compile -C builddir
            meson install -C builddir
        "
        # PG is stopped during builds; the .so loads on the final restart.
        echo "=== PRISM installed ==="
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
        # PG is stopped during builds; the .so loads on the final restart.
        echo "=== VectorChord installed ==="
    fi
fi

# --- ParadeDB (pg_search) ---

if [ "$PARADEDB" = "1" ]; then
    packages=()
    # A manual `infra provision` can opt in on a box that skipped pgvector.
    if [ ! -f "/usr/share/postgresql/${PG_VERSION}/extension/vector.control" ]; then
        packages+=("postgresql-${PG_VERSION}-pgvector")
    fi
    if [ -n "$package_path" ]; then
        packages+=("$package_path")
    fi
    if (( ${#packages[@]} )); then
        echo "=== Installing pg_search dependencies and requested packages ==="
        sudo DEBIAN_FRONTEND=noninteractive apt-get install -y "${packages[@]}"
    fi
fi

# --- Connection service file ---

# Register the benchmark instance in ~/.pg_service.conf so any libpq client
# connects with `psql service=rekall` (or PGSERVICE=rekall) instead of
# spelling out host/port/user/password. Credentials match the CREATE USER in
# cloud-init. Idempotent: written once; delete the [rekall] section to
# regenerate.
if ! grep -qE '^\[rekall\]' "$HOME/.pg_service.conf" 2>/dev/null; then
    cat >> "$HOME/.pg_service.conf" <<SVCEOF
[rekall]
host=localhost
port=$PG_PORT
dbname=rekall
user=rekall
password=rekall
SVCEOF
    chmod 600 "$HOME/.pg_service.conf"
    echo "=== Wrote ~/.pg_service.conf ([rekall] service) ==="
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

        # See the pg_vectorsearch block: GIT_SSH_COMMAND only for the initial
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
    BENCH_DIR="$HOME/benchmarks"
    rekall setup --force \
        --data-dir "$BENCH_DIR/data" \
        --results-dir "$BENCH_DIR/results" \
        --dsn "host=localhost port=$PG_PORT dbname=rekall user=rekall password=rekall"
fi

echo "=== Starting PostgreSQL ==="
sudo systemctl start "$PG_UNIT"
wait_for_postgres

preloads=()
if [ "$VECTORCHORD" = "1" ]; then preloads+=(vchord); fi
if [ "$PARADEDB" = "1" ]; then preloads+=(pg_search); fi
if (( ${#preloads[@]} )); then
    current=$(pg_admin -Atc 'SHOW shared_preload_libraries')
    merged=$current
    for lib in "${preloads[@]}"; do
        if ! has_preload_library "$merged" "$lib"; then
            merged="${merged:+$merged,}$lib"
        fi
    done
    if [ "$merged" != "$current" ]; then
        # ALTER SYSTEM preserves PostgreSQL's precedence over included files.
        # Keep the previous file so a bad preload cannot strand the server.
        auto_conf="$(pg_admin -Atc 'SHOW data_directory')/postgresql.auto.conf"
        backup=$(sudo mktemp "${auto_conf}.rekall.XXXXXX")
        sudo cp -p -- "$auto_conf" "$backup"
        auto_backup=$backup
        pg_admin -v "preloads=$merged" <<'SQL'
-- PostgreSQL treats one SQL string containing commas as one library.
-- Emit a separate SQL value for each item in the configured list.
SELECT 'ALTER SYSTEM SET shared_preload_libraries TO ' ||
       string_agg(quote_literal(
           CASE WHEN left(item, 1) = '"' THEN
               replace(substr(item, 2, length(item) - 2), '""', '"')
           ELSE item END
       ), ', ' ORDER BY pos)
FROM regexp_matches(:'preloads', '(?:^|,)[[:space:]]*("(?:[^"]|"")*"|[^",]+)', 'g')
     WITH ORDINALITY AS matches(parts, pos)
CROSS JOIN LATERAL (SELECT btrim(parts[1]) AS item) AS library
\gexec
SQL
        sudo systemctl restart "$PG_UNIT"
        wait_for_postgres
    fi
    current=$(pg_admin -Atc 'SHOW shared_preload_libraries')
    for lib in "${preloads[@]}"; do
        if ! has_preload_library "$current" "$lib"; then
            echo "ERROR: PostgreSQL did not preload $lib" >&2
            exit 1
        fi
    done
    if [ -n "$auto_backup" ]; then
        sudo rm -f -- "$auto_backup"
        auto_backup=
    fi
fi
recover_postgres=0

echo "=== Provisioning complete ==="
echo ""
echo "Connect to the benchmark database with: psql service=rekall"
echo ""
echo "Repos cloned to ~/repos/. To update later:"
if [ -n "$PG_VECTORSEARCH_GIT_URL" ]; then
    echo "  cd ~/repos/pg_vectorsearch && git pull && sudo meson compile -C builddir && sudo meson install -C builddir"
fi
if [ "$VECTORCHORD" = "1" ]; then
    echo "  cd ~/repos/vectorchord && git pull && cargo pgrx install --release --pg-config=$PG_CONFIG"
fi
if [ "$PARADEDB" = "1" ]; then
    echo "  pg_search: set [paradedb] version, run \`rekall infra provision\`, then ALTER EXTENSION pg_search UPDATE"
fi
if [ -n "$REKALL_GIT_URL" ]; then
    echo "  cd ~/repos/rekall && git pull && source ~/.cargo/env && cargo build --release && sudo cp target/release/rekall /usr/local/bin/"
fi
