#!/bin/bash
# Verify every tracked Rust source file carries the PostgreSQL License header.
#
# Usage: ./scripts/ci/check-license-headers.sh [file...]
#   With no arguments, checks every git-tracked *.rs file. Pre-commit
#   passes the specific files it's checking as arguments instead, so this
#   also works as a per-file pre-commit hook.
#
# A file passes if "Licensed under the PostgreSQL License" appears
# anywhere in its first 5 lines -- matching this project's header
# convention (see LICENSE and any existing source file):
#
#   // Copyright (c) <year> Tiger Data, Inc.
#   // Licensed under the PostgreSQL License. See LICENSE for details.

set -euo pipefail

if [[ $# -gt 0 ]]; then
    files=("$@")
else
    mapfile -t files < <(git ls-files -- '*.rs')
fi

missing=()
for f in "${files[@]}"; do
    if ! head -n 5 "$f" | grep -q "Licensed under the PostgreSQL License"; then
        missing+=("$f")
    fi
done

if [[ ${#missing[@]} -gt 0 ]]; then
    echo "==> Missing PostgreSQL License header:" >&2
    printf '    %s\n' "${missing[@]}" >&2
    exit 1
fi

echo "==> All ${#files[@]} source file(s) carry the license header"
