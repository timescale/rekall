#!/bin/bash
# Check that every commit in a range has a Conventional Commits message.
#
# Usage: ./scripts/ci/check-commit-messages.sh [<rev-range>]
#   <rev-range>  Commits to check, e.g. origin/main..HEAD (the default) or
#                "$BASE_SHA..$HEAD_SHA" in CI.
#
# Each message goes through the conventional-pre-commit hook configured in
# .pre-commit-config.yaml, so the rule set (and the allowed types) is the
# same one the commit-msg hook applies locally.

set -euo pipefail

range=${1:-origin/main..HEAD}
msg=$(mktemp)
trap 'rm -f "$msg"' EXIT

failed=0
checked=0
while read -r sha; do
    checked=$((checked + 1))
    git log -1 --format=%B "$sha" > "$msg"
    if ! pre-commit run conventional-pre-commit --hook-stage commit-msg \
            --commit-msg-filename "$msg" > /dev/null 2>&1; then
        echo "==> $(git log -1 --format='%h %s' "$sha")" >&2
        failed=1
    fi
done < <(git rev-list --reverse "$range")

if [[ $failed -ne 0 ]]; then
    echo "The subject(s) above are not of the form <type>[(scope)]: <summary>;" >&2
    echo "see https://www.conventionalcommits.org/ and the types listed in" >&2
    echo ".pre-commit-config.yaml. Reword the commit(s) and force-push." >&2
    exit 1
fi

echo "==> All $checked commit message(s) in $range follow Conventional Commits"
