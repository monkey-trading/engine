# Git workflow

Load on request before creating a branch, committing, pushing, or opening or updating a PR.

**Rule**: Branch, commit, verify, and push the same way every time.

## Branches

- GitHub flow: feature and fix branches → protected `main`. Never commit directly to `main`.
- One feature never spans several pushed branches or stacked PRs
  ([agent-build-feature](../skills/agent-build-feature/SKILL.md)). Local branches that merge into
  the feature branch, such as one per subagent worktree, are fine.
- Rebase a branch on the latest `main` before opening its PR; the PR lands as one squashed
  commit.
- Name new branches `{gh user}/<slug>[-gh<issue>...]`:
  - `{gh user}` — the operator's GitHub login, from `gh api user --jq .login`.
  - `<slug>` — 2–5 kebab-case words, max 40 chars, `a-z 0-9 -`, no leading or trailing hyphen.
    Drop filler words.
  - `gh<issue>` — the GitHub issue number the branch works on (e.g. `gh12`). Ask whether there is
    one when the request does not say; omit it when the user confirms there is none. Several
    issues are appended in order.
- An existing branch, including a PR's head branch, keeps its original name.
- A new branch starts with no fix notes: whoever creates it removes any
  `agents/.processes/fixes/<branch>.md` left in the main checkout
  ([agent-code-review](../skills/agent-code-review/SKILL.md#fixing-an-accepted-finding)), so a
  reused name inherits no stale invariants.

```
{gh user}/size-spread-tiers
{gh user}/size-spread-tiers-gh12
{gh user}/size-spread-tiers-gh12-gh15
```

## Commits

- Commit only finished work, after the user approves the exact message — unless they already told
  you to commit. One commit per coherent change.
- Message: `[<scopes>] Subject`, optionally followed by a blank line and `-` bullets of logical
  changes. The squash builds the `main` commit from the PR title and body, so branch commits may
  stay one line; the detail belongs in the PR body.
  - Scopes are the areas touched, comma-separated: `[core]` (`crates/core`), `[node]`
    (`crates/node`), `[config]`, `[contracts]`, `[agents]`, `[workspace]` (root manifests and
    tooling) — e.g. `[core,node]`.
  - Subject: imperative, specific, ≤ 72 chars including the tag, no trailing period.
  - Bullets use `-`, never `*`, and start with a verb such as `Add`, `Update`, `Fix`, `Remove`, or
    `Refactor`; logical changes, not file-by-file detail.
  - State what the commit does; no narration of discussion, design history, or review.
- No AI signatures anywhere — commits, PR titles and bodies, or GitHub comments: no
  `Co-Authored-By`, `Generated with`, or tool badges.

## Local checks before push

Before a push that opens or updates a PR, check only what the change affects. On the PR, CI runs
every job the changed paths can affect, and skips the rest; a push to `main` runs every job.

- Take the validation commands of the `.github/workflows/` jobs the changed paths trigger — format
  check, build or lint, and the tests covering the change — and run them scoped to the affected
  crates, each exiting zero. Run formatters as checkers, never as writers. In
  [ci.yaml](../../.github/workflows/ci.yaml) those are `cargo fmt --all -- --check`,
  `./scripts/check-file-loc.sh`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test -p <crate>` for each affected crate, `./scripts/check-reference-logs.sh` when
  `crates/core/tests/reference/` changes, and `cargo deny check` when dependencies change.
- A push to a draft PR may carry failing or skipped checks when the PR says which; `ci-passed`
  passes before the PR is marked ready.
- When the change touches a shared or public surface, add its direct consumers, since a check
  scoped to one crate never builds them: a change to `engine-core`'s public API also checks
  `engine-node`.
- Leave fuzzing, benchmarks, and long-running replays to CI.
- Never run a publishing or mutating step: registry logins, image builds that push, releases,
  deploys, cache or artifact uploads, or repository writes.
- [context-discipline](context-discipline.md) counts a request to open or update the PR as
  approval for these checks; keep their output out of the prompt as it says.
- After the push, the PR's CI is the full check of the changed paths: read its result before
  asking for review.

## Push

- Push only on a request for that push; a request to make or commit changes is not one. A request
  to open or update a PR is.
- Read every check result before pushing. Never chain checks and the push in one command: a
  pipeline or a grep over a log can exit zero on a failed run.
- Force-push only your own unreviewed history, and only with `--force-with-lease`.
