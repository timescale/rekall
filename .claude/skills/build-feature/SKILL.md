---
name: build-feature
description: >
  End-to-end workflow for a new feature: branch, plan, review the plan,
  implement, adversarially review the finished branch, then open a PR.
  Trigger when the user asks to build, implement, or ship a new feature
  end to end rather than make a small isolated change.
---

# Building a feature end to end

Orchestrates the `git-workflow`, `testing`, `review`, and `create-pr`
skills around a research → plan → review → implement → review pipeline.
Each stage below is a confirmation point — surface what came out of it and
get a go-ahead before moving to the next, rather than silently chaining
all the way to an opened PR.

For a small, well-understood change, this whole skill is overkill — just
use `git-workflow` and `create-pr` directly. Reach for this one when the
feature is substantial enough that planning and review add real value: a
new index backend, a new phase or output sink, a change to how a metric
is computed, or anything the `infra` commands do to a cloud account.

## 1. Start the branch

Use the `git-workflow` skill to start a `feature/<name>` branch off an
up-to-date default branch.

## 2. Research prior art

A similar or identical feature may already have been attempted, partly
built, or deliberately abandoned with a reason that isn't written down
anywhere else. Check before planning:

- `git branch --all` and `git log --all --oneline --grep=<keyword>` for
  branch names or commit messages suggesting a related attempt, local and
  on remotes.
- `git log --all --oneline -- <path>` for commits touching the files this
  feature will likely touch, even on branches that were never merged.
- `gh issue list --search <keywords>` and `gh pr list --state all --search
  <keywords>` — a past PR often carries design discussion, a rejected
  approach, or a caveat that never made it into docs or code comments.
- `docs/`, the suites under `benchmarks/` (a config there
  may already exercise what the feature is for), and any project memory
  covering the area.

Bring back to the user what's already been tried and what happened to it
(merged, abandoned, superseded), and flag anything worth reusing or
cherry-picking rather than reimplementing — including code sitting on an
old, unmerged branch. Carry the findings into the plan in step 3 so the
reasoning survives past this session, rather than treating this as a
throwaway search.

## 3. Plan the feature

For a bug fix or a very small, well-understood change, the plan-and-review
stage (this step and step 4) may not be worth the overhead — but ask the
user rather than deciding to skip it yourself.

Produce a step-by-step implementation plan before writing any code,
informed by step 2's research. Ground it in the actual codebase — the
`IndexBackend` trait and its opaque params, the phase split in
`rekall-lib/src/phase/`, the output sinks, this repo's CLAUDE.md
conventions, and any files/docs the user already pointed at — not just a
restatement of the user's request. Identify the files that will change
and the key design decisions, including alternatives considered and why
they were rejected.

The plan's design decisions should be driven by the priority order in the
`review` skill (measurement fidelity, then scalability, then safety, then
simplicity/architecture/clean code, including checking for duplication).
This ordering matters when trade-offs are unavoidable: don't propose a
simpler design that adds work inside a timed region, holds a dataset in
memory, or widens what a phase drops without calling that trade-off out
explicitly and letting the user weigh in, rather than defaulting to
"simple" silently.

Think about scalability, both in terms of design and implementation. The
plan must include a strategy for breaking big inputs into bounded pieces:
memory must not grow with the dataset, the number of queries, or the
length of a run. Stream through bounded channels, batch, and release each
batch before the next, the way the loader does.

Name the config keys, CLI flags and result-file fields the feature adds.
They are public surface (see the `review` skill), so their names and
defaults are design decisions, and `docs/configuration.md` changes with
them.

## 4. Review the plan

Have an independent agent review the plan — fresh context, not the one
that authored it, so it isn't reasoning from the same assumptions —
following the `review` skill's "Reviewing a plan" section.

Bring what it finds back to the user. Revise and re-review if there are
material concerns; get explicit go-ahead before implementing.

## 5. Implement

Work in small, focused, reviewable increments — commit as you go rather
than producing one large diff at the end, and don't hesitate to use a
throwaway helper branch to stage a complex sub-part (see `git-workflow`
step 2). This in-progress history doesn't need to be clean; it gets
tidied into its final shape in step 7, per `create-pr`. Pause for user
review after each meaningful step rather than implementing the whole
feature unattended, unless the user has explicitly asked for larger
unattended work.

Write tests alongside the code, not as an afterthought once everything is
"working" — use the `testing` skill to pick the right kind of test and
where it belongs. A feature isn't actually finished until its edge cases
have test coverage, not merely once the happy path works and it compiles.
Run the relevant tests as you go rather than waiting until step 7's
pre-commit checks to discover something is broken, and run the
integration tests against a real PostgreSQL when the feature touches a
backend or a phase — CI cannot.

Keep memory bounded as you go, not as a cleanup at the end: a `Vec` that
grows with rows, queries or results is the thing to catch while writing
it. Verify against a dataset large enough to notice, not only the
smallest config that runs.

## 6. Adversarially review the branch

Once the feature is finished, or nearly so, have a fresh review agent —
not the one that implemented it — review the diff against the plan and
this codebase's conventions, following the `review` skill's "Reviewing
code" section (correctness and tests, code cleanliness, documentation
consistency, safety, measurement and scalability).

Report findings to the user and fix what's confirmed real — don't paper
over findings just to get to the PR, and if something is a deliberate
trade-off rather than a bug, say so instead of silently dismissing it.

## 7. Open the PR

Hand off to the `create-pr` skill for pre-commit checks, committing,
rebasing, pushing, and opening the PR — including tidying the branch's
history into its final shape (single commit, or a small number of
self-contained ones) per that skill's "Final commit history shape"
section before pushing.
