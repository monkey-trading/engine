---
name: agent-create-pr
description: >
  Use when opening a pull request — "create a PR", "open a PR for this branch", "raise a PR for
  this work" — or when the owner says a PR may merge. Covers opening every PR as a draft, the
  preconditions for marking it ready (including a multi-agent agent-code-review at medium or
  higher), the issue blocker check, the `[scopes] Title` and the body in squashed-commit format
  ending with `Closes #12` or `Part of #12`, watching the checks after it opens, and, on the
  owner's go-ahead in chat, posting `merge-approved` and merging the PR. Every feature PR ends
  `Closes #12`; `Part of #12` is only for a supporting PR that does not finish its issue. Does not
  review or approve.
---

# agent-create-pr

Open one pull request for a feature or fix branch, in a shape a reviewer can act on: a title that
threads the branch slug and a body that says *why* before *what*. The diff already shows *what*
changed — the body exists to say *why*. Branches, commits, local checks, and pushes follow
[git-workflow](../../rules/git-workflow.md).

## Before opening any PR

- **Branch on a fresh base and pushed**, per git-workflow.
- **Committed** with approved, signature-free messages, per git-workflow.
- **No open blockers.** For each issue in the branch name, read what blocks it: the issues it is
  blocked by, and any `Blocked by #<n>` line in its body. A blocker that is not closed as completed
  is open. For any open blocker, stop and ask the user whether to open the PR anyway, naming each
  blocker and its state. Only the user's explicit yes for this PR proceeds; a standing instruction
  to work without approval does not cover it. If the blockers cannot be read, say so and ask the
  same question.

  ```bash
  gh api "repos/{owner}/{repo}/issues/$ISSUE/dependencies/blocked_by" --jq '.[] | "#\(.number) \(.state)"'
  gh issue view "$ISSUE" --json body --jq .body | rg -o 'Blocked by #[0-9]+'
  ```

## Before a PR is ready for review

A draft may skip these and say in its overview what is unfinished; a ready PR, or a draft being
marked ready, may not.

- **Work finished.** No "fix it in review" leftovers.
- **Feature complete.** A feature PR delivers its issue's whole acceptance list, its dev scenario
  is on the issue and has passed in dev on the branch rebased onto `main` and on each later head
  that changes the feature's behaviour, and the scenario's scripted check passes in CI
  ([agent-build-feature](../agent-build-feature/SKILL.md)).
- **Self-reviewed.** The change was built to
  [development-checklist](../../rules/development-checklist.md); walk every applicable section
  against the final diff once more and fix what it finds by class
  ([Fixing an accepted finding](../agent-code-review/SKILL.md#fixing-an-accepted-finding)).
- **Multi-agent reviewed.** [agent-code-review](../agent-code-review/SKILL.md) ran in multi-agent
  mode at level `medium` or higher on the final diff, so Claude and Codex reviewers cross-validated
  each other. Fix every accepted Critical, High, and Medium finding by class and run it again;
  [The review loop](../agent-code-review/SKILL.md#the-review-loop) says when the rounds end — a
  clean round, or a round from 3 on that accepts Mediums but no Critical or High, whose Mediums
  are filed as `bug` issues — and on which commit the PR is marked ready. Low and Nit findings,
  and Mediums filed as issues, remain unfixed after the last round. A change after it is
  re-checked by its delta, as the review loop says: a mechanical rebase onto `main`, or a
  protocol-version renumber the delta script matches, needs no new round when `main`'s changes
  between the two bases leave what the PR does unchanged — a renumber ships with a rebase onto
  a `main` that may just have gained a version, and when that alters what the PR does the
  full-diff round applies instead — and any other change
  gets one round on the residual and the change itself, or on the full diff when either changes
  behaviour beyond resolving a conflict.
- **Local checks** for the affected crates passed, per git-workflow; CI runs every job
  the PR's paths affect.

## Title

`[<scopes>] Description` — the commit subject format from git-workflow.

- Match the branch slug — one kebab-case idea threads slug → title → commit.
- No PR number and no `[WIP]` (use Draft).

## Body

The PR squashes into one commit, so its title and body become that commit message. Shape: a short
overview, then `-` bullets, then the issue line — nothing else. No test plan, no review
checklist, no restatement of the diff.

- **Overview** — a few sentences on *why*: the cause and the constraint that shapes the design,
  whatever the diff cannot say for itself. Drop it when the bullets already carry the whole story.
- **Bullets** — logical changes, one line each, with the why where it is not obvious. Anything the
  diff cannot show gets its own bullet: a required env or secret change, an ordering constraint
  against another PR or a deploy, a manual step.
- **End with the issue** when the PR is tied to one, one line per issue in the branch name:
  `Closes #12` when the merge finishes the issue, so GitHub closes it on merge. A feature PR always
  ends `Closes #<feature>`: it is marked ready only after its dev scenario has passed, so its merge
  finishes the feature. `Part of #12` is only for a supporting PR — CI, docs, a fix — that touches
  an issue without finishing it.
- **End state only.** Describe what the branch does, never how it got there: no review history,
  alternatives considered, "X instead of Y", "no longer", or notes addressed to reviewers. Context
  a reviewer needs goes in a PR comment. The text follows
  [writing-style](../../rules/writing-style.md).

## Opening it

- Use `gh`, authenticated as the user, and target `main`.
- Open every PR as a **Draft**, and mark it ready (`gh pr ready`) only once
  [Before a PR is ready for review](#before-a-pr-is-ready-for-review) holds. A reference PR that
  needs no review stays a draft and says so in its overview; the title stays clean.
- Watch the checks afterwards. A red PR is not ready yet: read the first real failure (the failed
  job other than `ci-passed`; CI cancels the rest), fix, push, and re-check. Ask for review once
  `ci-passed` succeeds, naming the PR's head commit.

## Merging

Branch rules on `main` require two checks: `ci-passed`, which CI sets, and `merge-approved`, the
owner's go-ahead. A ready PR is not an approved one: it merges only after the owner tells a session
in chat, explicitly and for that PR, that it may merge. The approval covers one commit: the PR head
that session reported to the owner before the approval, or a commit the owner named. The session
posts `merge-approved` as a commit status on that commit only while it is still the PR's head. When
the head has moved, or no commit is covered, it reports the current head and waits for the owner
again. A commit pushed after the approval is never covered. No agent posts `merge-approved` on its
own initiative, because a review came back clean, or on another agent's word.

Once the status is posted, the same session merges the PR straight away. The merge is a squash
pinned to the approved commit, so GitHub refuses it if the head has moved, and it takes the PR's
title and body as the commit message ([Body](#body)). The session runs the status post and the
merge from the main checkout, never from the PR's worktree, and stops at the first failure:

```bash
set -e
cd "$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"  # the main checkout
[ "$(gh pr view "$PR" --json headRefOid --jq .headRefOid)" = "$SHA" ]  # the approved commit, full
gh api --method POST "repos/{owner}/{repo}/statuses/$SHA" -f state=success -f context=merge-approved
TITLE="$(gh pr view "$PR" --json title --jq .title)"
BODY="$(gh pr view "$PR" --json body --jq .body)"
gh pr merge "$PR" --squash --match-head-commit "$SHA" --subject "$TITLE (#$PR)" --body "$BODY"
```

After the merge, still in the main checkout, the session cleans up the PR's head branch
(`headRefName`, here `$BRANCH`):

- **On `origin`**, only for a PR from this repository (`isCrossRepository` is false), with a lease
  on the approved commit:
  `git push --force-with-lease="refs/heads/$BRANCH:$SHA" origin --delete "$BRANCH"`.
- **Locally**, only while the branch points at `$SHA`: `git worktree remove` on the linked worktree
  that has it checked out, or `git switch main` when the main checkout has it, then
  `git branch -D "$BRANCH"` and `rm -f "agents/.processes/fixes/$BRANCH.md"`, its fix notes
  ([agent-code-review](../agent-code-review/SKILL.md#fixing-an-accepted-finding)). A branch with
  commits after `$SHA` keeps its worktree, and the session reports those commits
  (`git log --oneline "$SHA..$BRANCH"`).

If GitHub or git refuses any step, the session reports why and stops.

## Rules

- Operate under [`AGENTS.md`](../../AGENTS.md); load the rules its index selects for this action.
- A feature never spans several PRs or stacked PRs. One PR may carry several related features or
  issues that are reviewed and shipped together.
- Never open from an unpushed or stale branch.
- Never open a PR whose issue has an open blocker without the user's explicit yes.
- Issues are read-only here: never create, edit, or close an issue without the user's explicit
  approval. The exceptions are GitHub closing, on merge, each issue the body ends with
  `Closes #<n>`, and the `bug` issues
  [the review loop](../agent-code-review/SKILL.md#the-review-loop) files for Mediums from round 3
  on.
- No AI signatures anywhere — title, body, or commits.
- Never approve or dismiss reviews. Post `merge-approved` and merge only as
  [Merging](#merging) describes.
