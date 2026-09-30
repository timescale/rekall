---
name: create-pr
description: >
  Prepare and open a pull request end to end — pre-commit checks, a
  Conventional Commits commit, rebase, push, and a `gh pr create` body
  matching this repo's style. Trigger when the user asks to commit and open
  a PR, ship a branch, or "create a PR".
---

# Creating a pull request

Companion to the `git-workflow` skill, which covers starting a branch and
cleaning up after merge. This skill is the checklist for everything from
pre-commit checks through the opened PR, plus the PR body style used in
this repo.

Treat committing, pushing, and opening the PR as three separate confirmation
points — do not chain them into one silent sequence. Pause and confirm with
the user before each of the three, even if they asked to "create a PR" in one
go.

## 1. Check branch state

```bash
git status
git branch --show-current
```

If on the default branch, start a feature branch first — see the
`git-workflow` skill. Never commit directly to the default branch.

## 2. Run pre-commit checks

All of these must pass before staging anything. They are what CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace --locked
```

`pre-commit run --all-files` runs the same three plus the file checks and
is what the Lint job runs. `--locked` matters: a `Cargo.lock` that the
manifest no longer matches fails in CI even when it resolves locally.

Add to that, depending on what the branch touches:

- **Backends, load, index, search phases** — the integration tests, which
  need a live PostgreSQL and skip themselves silently without one:
  `REKALL_TEST_DSN="host=... dbname=..." cargo test --test pg_integration`
  (see the `testing` skill). A green `cargo test` without the DSN says
  nothing about them.
- **Workflows** — `zizmor --pedantic .github/workflows/` must report no
  findings.
- **The branch's commit messages** — `./scripts/ci/check-commit-messages.sh`
  runs the commit-msg hook over `origin/main..HEAD`, the same check CI
  applies to the pull request.

If a check fails, fix it before proceeding — don't open the PR with
known-failing checks hoping CI catches it later.

## 3. Stage and commit

Follow the `git-workflow` skill's instruction for committing.

- Stage files explicitly by name. Never `git add -A` or `git commit -a`.
- Split unrelated changes into separate commits.
- Review with `git diff --staged` before committing.
- **Confirm with the user before running `git commit`.**

### Final commit history shape

Whatever the branch's history looked like during development (see
`git-workflow` — lots of small WIP commits, checkpoints, even a helper
branch or two, is fine and encouraged there), the history a PR is opened
from should be either a single commit, or a small number of commits that
each stand on their own. One change, one commit is the default; several
commits are for several distinct changes that happen to ship together,
not for the stages one change went through.

Each commit should already be the final code it's introducing, not an
intermediate state that a later commit reworks. Concretely: avoid a commit
that adds function X followed later by a commit that refactors X — squash
those into one commit that adds the final X directly. A later commit doing
a small amount of refactoring *in order to build on top of* something
earlier (e.g. widening a function's parameters to fit new functionality)
is fine — that's forward progress, not undoing earlier work. The test is
directional: does each commit move the code toward its final state, or
does a later one take back something an earlier one did? The latter should
be squashed away with `git rebase -i` before proceeding, not left for the
reviewer to reconstruct from the diff.

Each commit in that final history should also build and pass the test
suite on its own — not just the branch's tip. A commit that introduces a
failure "temporarily," fixed by a later commit, keeps the code from being
bisectable: `git bisect` (and anyone reading history later) needs every
commit to be in a working state to be useful. If splitting the work into
several commits would leave an intermediate one broken, that's a sign
those commits should be squashed together instead. Run the checks from
step 2 against each commit, not only the last one, when in doubt — e.g.
`git rebase -i` with `exec cargo test --workspace --locked` between
commits.

Commit message format (Conventional Commits, no Co-Authored-By, no
machine-specific benchmark numbers, etc.) is covered in `git-workflow`'s
"Commit messages" section — it applies to these commits the same as any
other.

## 4. Rebase and push

```bash
git fetch origin
git rebase origin/main
```

Resolve any conflicts, then re-run the checks in step 2 if source files
changed during the rebase.

**Confirm with the user before pushing.**

```bash
git push -u origin <branch>                       # first push
git push --force-with-lease origin <branch>        # after amending/rebasing
```

## 5. Open the PR

**Confirm with the user before running `gh pr create`.**

Title mirrors the commit header style: `<type>(<scope>): <description>`.

Body — start with one or a few sentences describing the high-level idea
behind the PR. Often, just adding the commit message is enough.

Write prose, for humans, not a templated checklist, and keep it short: a
reviewer should get the point from the first sentence or two, not have to read
to the bottom to find out what the PR actually does.

- Lead with a snappy, high-level summary of what changed and why it
  matters — one or two sentences, before any detail. Someone skimming
  just the top of the description should already know what this PR is.
- Stay high-level throughout. This is a summary for a reviewer, not a
  transcript of the work — the diff already carries the detail; don't
  re-explain it line by line, and don't include every alternative you
  considered or every step you took to get there. Describe what the PR
  does now, not the history of how it got there.
- Only add a subsection per distinct change when there genuinely are
  several unrelated changes — organize by the reader's understanding, not
  by commit order. A PR with one coherent change doesn't need subsections
  at all.
- If there's a measurable before/after worth calling out (a benchmark
  that now completes, a report that now shows a column, a memory
  ceiling that now holds), add it in relative or qualitative terms. Never
  a raw QPS/latency number tied to one machine.
- No "Generated with Claude Code" footer, no Co-Authored-By, no local-only
  tool names.
- If the description is getting long, that's a signal to cut detail, not
  to add more structure to organize it.

```bash
gh pr create --title "<type>(<scope>): <description>" --body "$(cat <<'EOF'
<body>
EOF
)"
```

Report the PR URL back to the user when done.
