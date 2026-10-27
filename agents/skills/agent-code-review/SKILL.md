---
name: agent-code-review
description: Review a branch or GitHub PR against the team development checklist, with the current agent by default, or with independent Claude and Codex reviewers for each requested low, medium, or high review level. Writes local Markdown only; to post the findings on a GitHub PR, use agent-review-pr. Also says how any skill fixes an accepted finding — by class, not by instance — when a PR's review loop ends, and how a change after it is re-checked.
---

# agent-code-review

Load on request for a local, single-agent, or multi-agent code review of the current branch or a
GitHub PR, before fixing an accepted finding, to decide whether a PR's review loop has ended, and
to re-check a change made after it ended.
This skill creates local Markdown only: it never posts review comments or applies a
suggested fix. To draft the findings as comments on a GitHub PR, use
[agent-review-pr](../agent-review-pr/SKILL.md), which runs this skill.

## Files

Load in this order:

| File | When | Holds |
| --- | --- | --- |
| this file | every run, every fix of a finding | scope, review, final report, completion report, fixing an accepted finding, the review loop |
| [development-checklist](../../rules/development-checklist.md) | every run | everything a change must not miss |
| [references/severity.md](references/severity.md) | every run | how to grade a finding |
| [references/multi-agent.md](references/multi-agent.md) | multi-agent mode only | reviewer dispatch, cross-validation, consolidation |
| [templates/report.md](templates/report.md) | at completion | the user-facing report |

## Modes

- **Current-agent (default):** the agent running this skill reviews the scope and writes the final
  report. It sets both `Reported by` and `Validated by` to `Current agent` and dispatches no
  external agent.
- **Multi-agent:** independent Claude and Codex reviewers inspect the scope and cross-validate each
  other; the current agent writes the final context-aware assessment.

An explicit request for multi-agent review, or for a `low`, `medium`, or `high` review level,
selects multi-agent mode. A severity filter alone does not select a review level.

## Pashov audits

A Solidity diff or a multi-agent review request does not activate Pashov. Only an explicit
request naming a Pashov workflow routes through the
[Pashov routing and execution policy](../smart-contract-solidity-implementation/references/pashov.md).
Warn about its potentially large agent count and cost, then obtain approval of its run plan
before execution. Preserve this skill's current-agent default and review-level roster for
ordinary reviews; do not automatically add Pashov agents or duplicate audit passes.

## Scope

- **Branch scope (default):** `git diff <base>...HEAD`; `<base>` defaults to `origin/main`.
- **PR scope:** an explicit PR number or URL. Save its metadata, diff, and file list once with
  read-only `gh` commands.
- **Delta scope:** a delta round after the review loop ended ([The review loop](#the-review-loop)):
  the residual [scripts/review-delta.sh](scripts/review-delta.sh) prints, saved as
  `.residual.diff`, and the change itself in context, saved as `.change.diff`, with the full
  reviewed diff (`<reviewed-base>...<reviewed-head>`) saved as `.reviewed.diff` for context.
  `scope.md` records both base and head pairs. The round reads source at the new head, and the
  next re-check starts from its new base and head.

Honor a user-provided base ref, path subset, or commit range exactly. In PR scope, re-check the
head SHA immediately before the review begins. Use only read-only `gh` commands, and never print or
persist the GitHub credential. A caller such as `agent-review-pr` may hold a write-capable one, so
the no-post rule below is the safeguard.

Create the gitignored `agents/.processes/reviews/<YYYY-MM-DD-HHMM>-<kebab-slug>/` and save the
scope before any reviewer starts:

```bash
review_base_sha=$(git rev-parse <base>)
review_head_sha=$(git rev-parse HEAD)
review_args=("$review_base_sha...$review_head_sha")
# For an explicit range, set review_args=("$requested_range") instead.
# For a requested path subset, append with: review_args+=(-- "${requested_paths[@]}")
git diff "${review_args[@]}" > <run-dir>/.branch.diff
git diff --stat "${review_args[@]}" > <run-dir>/.branch.stat
```

For PR scope:

```bash
gh pr view <pr> --json number,title,baseRefName,headRefName,headRefOid,headRepository \
  > <run-dir>/.pr.json
gh pr diff <pr> > <run-dir>/.pr.diff
gh pr diff <pr> --name-only > <run-dir>/.pr.files
```

Record `scope.md`: scope type, mode, review levels, range and paths, base ref, resolved SHAs, diff
and stat paths, and the commands that created them. In branch scope, read surrounding source from
the recorded head SHA (`git show "$review_head_sha:<path>"`), not the live worktree. In PR scope,
read it from a local checkout at the recorded head SHA; ask before fetching that SHA.

## Review

Read [AGENTS.md](../../AGENTS.md) and load every rule and implementation skill it selects for the
diff. Walk every applicable section of the development checklist in order, design first, and grade
each finding with the severity guide. In current-agent mode, re-check each accepted finding against
its source before writing the final report.

In either mode, the current agent also checks that each invariant in the fix notes of the branch
under review — in PR scope, the PR's head branch — holds across the diff
([Fixing an accepted finding](#fixing-an-accepted-finding)). A site where one fails is a finding,
unless the commits or the user show that the branch changed that invariant on purpose.

## Final report

Write `code-review-final.md` with accepted findings only, grouped under `## Critical`, `## High`,
`## Medium`, `## Low`, and `## Nit`, simplest complexity first in each group:

```markdown
### <Name — short imperative title>
- **Severity:** Critical | High | Medium | Low | Nit
- **Complexity:** Trivial | Small | Medium | Large
- **Confidence:** High | Medium | Low
- **Reported by:** Current agent | `<reviewer>`; list every reporting reviewer
- **Validated by:** Current agent | `<reviewer>: Confirmed | Rejected | Needs context`; list every validator
- **File(s):** `<path>:<line>` (or `<path>:<start>-<end>`)
- **Issue:** 1-3 sentences.
- **Scenario:** the sequence of events that triggers it, or `n/a` for a style finding.
- **Suggested fix:** 1-3 sentences.
```

## Completion report

Print [templates/report.md](templates/report.md) filled in. Replace every `{{…}}` so none remains,
keep every label and its order except where a rule below says otherwise, and fill it by these
rules:

- **Target:** `Branch <branch> vs <base>` or `PR #<number>`. **Title:** the PR title, or the
  branch's top commit subject. **Issues:** every GitHub issue in the branch name (`gh<n>`) or PR
  body (`#<n>`), or `no issue`.
- **Overview:** one or two sentences on what this review found, never on what the change does —
  `Why` and `What` cover that. It is the reason for the verdict: name the main problems and where
  they sit, or say that nothing blocks. Write `Two blocking problems in the retry path: a double submit on timeout and no
  idempotency key.`, not `This PR adds retries to order submission.` No restated finding text, no
  praise.
- **Why:** one or two sentences on the problem and the intended outcome, from the PR description or
  commits and the diff. Never invent intent.
- **What:** two to four bullets of behavior or data-flow changes, not files.
- **Fit to issue:** what the GitHub issue asks that the change covers, misses, or exceeds. Read it
  with `gh issue view <n> --comments`; say `no issue`, or `not checked — <reason>` when it cannot
  be read.
- **Risk:** one line — affected crates and flows, log or state-root compatibility, config / secret
  changes, deploy ordering against the contracts, rollback.
- **Tests:** one line — what the change's own tests cover and the notable gap.
- **Verdict:** `REQUEST CHANGES` when an accepted finding is Critical, High, or Medium, otherwise
  `APPROVE`, and nothing else; the `Overview` is its reason.
- **Findings:** one item per accepted finding, in `code-review-final.md` order, each field on its
  own sub-item. Keep `Reported by` and `Validated by` only in multi-agent mode. With no findings,
  the list is `none`.
- **Rejected:** multi-agent mode only — each candidate `consolidated.md` left rejected or needing
  context, with its reason. Drop the block in current-agent mode.
- **Range / Diff / Loaded:** the resolved range (or head SHA), the diff stat, and the rules and
  skills loaded.
- **Not verified:** what the review did not run or check — tests, log replay, CI, runtime behavior.
  `nothing` only when true.
- **Mode / Agents / Current agent:** `current-agent` or `multi-agent`; `Current agent only` or each
  review level and reviewer model; the provider, model, and effort writing the final assessment.
- **Final review:** the absolute path to `code-review-final.md`.

A calling skill that prints its own report built from these fields skips this one.

## Fixing an accepted finding

This skill never fixes code. Whoever fixes an accepted finding — in a self-review, in the review
loop before a PR is marked ready ([The review loop](#the-review-loop)), or when addressing PR
comments ([agent-address-pr-comments](../agent-address-pr-comments/SKILL.md)) —
fixes its class, not only the cited instance:

1. **Name the invariant** the finding breaks, in one checkable sentence: "a refused signed request
   burns its nonce", "rounding favours the house", "every `select!` arm stays live".
2. **Fix every site.** Find every place in the PR's code, and in the code it touches, where that
   invariant can fail, and fix them all, not only the cited line. Check both directions and both
   sides wherever they apply: long and short, read and write, success and error, startup and steady
   state.
3. **Test the class.** Add a test that covers the class, as a property or table test where one
   fits, not only the cited scenario, and mutation-check it as
   [development-checklist](../../rules/development-checklist.md#13-tests) requires. A finding no
   test can cover, such as wording, skips this step.
4. **Re-read the fix diff** before the next review round, against the finding's scenario and its
   neighbours, to check that the fix creates no mirror-image defect.
5. **Record the invariant and the sites fixed** in the branch's fix notes, under a heading for the
   round, so every later round checks the class instead of rediscovering it. A fix made outside a
   numbered round — in a self-review or when addressing PR comments — goes under a heading naming
   its source and the next round, numbered from the round log ([The review loop](#the-review-loop)),
   such as `Self-review, before round 1` or `PR comments, before round 4`. An entry stays only
   while the branch means its invariant to hold: any change that alters a recorded invariant — a
   later fix, a revert, a rejected fix, or a change the user asks for — rewrites or removes the
   entry. The notes are the gitignored
   `agents/.processes/fixes/<branch>.md` in the main checkout, which every worktree finds with
   `dirname "$(git rev-parse --path-format=absolute --git-common-dir)"`, with the PR's head branch
   name kept whole (`fixes/<user>/size-spread-tiers-gh12.md`). Only the agent that owns the PR
   branch writes them; a subagent returns its invariant and sites to that agent. A new branch
   starts with none ([git-workflow](../../rules/git-workflow.md#branches)).

Every round but a delta round ([The review loop](#the-review-loop)) reviews the full diff; the
recorded invariants add to what it checks and never narrow it.

## The review loop

A PR is marked ready through review rounds, numbered from the first on its branch. Each round runs
this skill in multi-agent mode at `medium` or higher over the PR's full diff
([agent-create-pr](../agent-create-pr/SKILL.md#before-a-pr-is-ready-for-review)), or in a delta
round over the residual and the change itself, and every accepted Critical, High, and Medium
finding is fixed by class before the next round.

- **A bug fix gets a single-agent check.** A PR that closes an issue labelled `bug` does not run
  the multi-agent loop. The owning agent reviews its own full diff once in current-agent mode
  against the development checklist and the severity guide, fixes every accepted finding by class
  (there is no multi-agent round, so the round-3 Medium deferral does not apply — Mediums from
  this check are fixed in the PR too), and records one round-log row naming the single-agent
  check, its findings, and its fixes. A later change is still re-checked by its delta below. The
  PR is marked ready once `ci-passed` is green on the head, as after a clean round. Feature PRs
  and protocol-version PRs always run the full loop.
- **The round log is the count.** The branch's fix notes
  ([Fixing an accepted finding](#fixing-an-accepted-finding)) open with a round log: one line per
  round with its number, the commit it reviewed, and its accepted counts by severity. A session
  reads it before anything else on the branch, numbers the next round from it, and counts the
  Medium cap from it.
- **A clean round ends the loop.** A round that accepts no Critical, High, or Medium finding ends
  it. After it, change no code, fix no Lows or Nits, and run no further rounds. Mark the PR ready
  on exactly the commit that round reviewed, or on a later one that needs no round under the
  re-check below, once `ci-passed` is green on it; the remaining Lows and Nits go in the report to
  the owner, not into new commits.
- **A later change is re-checked by its delta.** A mechanical rebase onto `main`, one that
  resolves no conflict and leaves the PR's behaviour unchanged, needs no re-check; the rebased
  commit stands in for the reviewed one. After any other change following the round that ended
  the loop, run [scripts/review-delta.sh](scripts/review-delta.sh) with the base and head that
  round reviewed (its `scope.md`) and the new ones:
  `<reviewed-base> <reviewed-head> <new-base> <new-head> [OLD=NEW ...]`. It compares the reviewed
  patch with the new one, each taken against its own base so `main`'s changes drop out, and
  ignores reference logs and changed lines that enumerate two or more version labels (the
  all-version prose lists a renumber also touches, which `main` extends with its own version). For
  a renumber, the pairs rename in the reviewed patch the PR's version
  labels (`V21=V23 v21=v23`) and, when `main` has gained versions, its previous latest
  (`V20=V22 v20=v22`), so a site the renumber missed shows. The script exits 0 when the patches
  match and 1 with the residual printed; any other exit is a failure to fix before relying on it.
  Record in the round log which re-check ran (no round, a delta round, or a full-diff round) and
  the script's exit status.
- **A renumber the script matches needs no round.** A protocol-version renumber, with the
  reference logs it generates, needs no review round when the script exits 0 with its label
  pairs and `main`'s changes between the two bases leave what the PR does unchanged. A renumber
  ships with a rebase onto a `main` that may just have merged another version whose rules then
  run with this PR's; that is a behaviour change, and the full-diff rule below takes precedence
  over this grant. The renumber, and the rebase it comes with, change only labels and reference
  logs and resolve conflicts only where versions are listed: the version enum, `LATEST`, and
  the lists of versions and reference logs. Anything else, such as a conflict resolved in the
  feature's code, makes it another change. The PR still needs its local checks
  ([git-workflow](../../rules/git-workflow.md#local-checks-before-push)) and a green `ci-passed`
  on the new head.
- **Any other change gets a delta round.** A rebase that resolves conflicts or changes the PR's
  behaviour, a fix for a failed check or dev scenario, a fix for a review comment on a ready PR, a
  change the user asks for, or any other change to the patch moves the PR back to draft and
  reopens the loop for one delta round: the same reviewer pair, in fresh sessions and
  cross-validated as usual, reviews the residual the script prints and the change itself in
  context (the new diff of each file where a rebase resolved a conflict, and each commit the
  change added), with the full reviewed diff as context. The residual has no context lines, so it
  cannot show where an unchanged line now sits; the round runs even when the script exits 0. It
  is numbered in the round log, and the rule below for Mediums from round 3 applies to it.
- **A behaviour change needs a full-diff round.** When the residual or the change itself changes
  behaviour beyond resolving a conflict, such as a new code path, changed semantics in the
  feature's own logic, or a rebase whose `main` changes alter what the PR does, the round reviews
  the PR's full diff instead.
- **Critical and High have no round cap.** Keep fixing and re-reviewing until a round accepts none.
- **From round 3, Mediums go to bug issues.** A round numbered 3 or later — including one in a
  reopened loop — that accepts Mediums but no Critical or High ends the loop as a clean round
  does, and its Mediums are not fixed in the PR. The agent that owns the PR files each as its own
  GitHub issue labelled `bug`, to be fixed soon after in a small PR of its own. This rule approves
  filing them, so they need no per-issue yes from the user, as an exception to the issue-write
  rules of agent-create-pr, agent-build-feature, and agent-address-pr-comments. Each issue body is
  a one-line summary, then `**Where:**` with the `file:line` and
  `Found by review round <N> of #<PR>.`, then `**Scenario:**` and `**Expected:**`. Record each in
  the fix notes, after the round log, with its title, `file:line`, scenario, and issue number. Then
  mark the PR ready on exactly the commit that round reviewed, or on a later one that needs no
  round under the re-check above, once `ci-passed` is green on it, and list the issues in the
  report to the owner.
- **A filed Medium stays with its issue.** A later round that finds a recorded filed Medium again,
  with the same scenario and no greater impact, notes its existing issue in its report instead of
  raising it again. When a later change widens its scenario or raises its impact, the round grades
  it afresh by the severity guide.

## Non-negotiable rules

- Walk every applicable checklist section; name any you could not walk under `Not verified`.
- Every finding is evidenced in code, config, or tests at the recorded head, and every finding but
  a style one names the scenario that triggers it; a finding with no reachable scenario is dropped.
- A safety or process rule violation is at least High and an active bypass is Critical; any
  other rule or skill is graded by its impact, per the severity guide.
- Local Markdown only: never post a review comment, and never apply a suggested fix without an
  explicit follow-up request.
