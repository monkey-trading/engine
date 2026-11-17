---
name: agent-security-fix
description: >
  Use when fixing, landing or recording a confirmed security finding — anything a security audit
  or a private report found that must not be public before its fix lands. Covers recording a
  finding as a draft repository security advisory, keeping every artefact private, building the
  fix in the advisory's temporary private fork, landing it through the private CI mirror on the
  owner's press of the advisory's merge button, and cleaning up. Does not cover ordinary bug PRs
  (agent-create-pr) or reviewing a public PR.
---

# agent-security-fix

Fix and land a confirmed security finding without publishing it before its fix is on `main`. The
repository is public, so everything about an unpublished finding stays in the working record, the
advisory and its private fork. Ordinary PRs go through [agent-create-pr](../agent-create-pr/SKILL.md);
this skill replaces its branch, push, review-posting and merge steps for a finding, and keeps its
local review and check rules.

## Recording a finding

- A finding becomes an advisory only after it has been reproduced independently, outside the
  session that found it.
- Each finding is one draft repository security advisory:
  `POST /repos/{owner}/{repo}/security-advisories`, with a summary, a description, a severity and
  the affected component.
- The working record (reports, exploit tests, the findings ledger) lives only in the main
  checkout's gitignored `agents/.processes/` tree, never in a tracked file.
- Only the coordinator session creates advisories. A subagent never writes a finding to GitHub.

## Confidentiality

- Never push a fix branch to `origin`.
- Never open a public PR, issue or comment about an unpublished finding.
- Keep every file, PR bodies included, in your own worktree under a unique name. Never use a shared
  scratch folder: a shared PR-body file once put a private fix's text into a public PR's edit
  history.
- Read a body file back before any `gh pr`, `gh issue` or `gh api` write, and check it is your own
  text.
- Git remotes are shared across worktrees. Name each private fork's remote `private-<first four
  characters of its GHSA id>`, and check its URL with `git remote get-url` before every push.
- If private text ever reaches a public page, scan that page's edit history through GraphQL
  `userContentEdits`. The owner deletes the revision in the browser, because no API can.

## Building the fix

- Work in a worktree of your own, on a branch pushed only to the advisory's temporary private fork,
  created with `POST /repos/{owner}/{repo}/security-advisories/{ghsa}/forks`.
- **Only one open PR per advisory may target `main`.** GitHub blocks the advisory's merge
  otherwise. Related advisories fixed together live in one advisory's fork. A follow-up fix to the
  same advisory opens its PR only after the first has merged.
- The exploit tests become regression tests, named by the behaviour they hold, failing before the
  fix and passing after it.
- Write one mutation per fix and revert it afterwards.
- A temporary fork runs no CI. Run locally what CI would run for the touched areas, per
  [git-workflow](../../rules/git-workflow.md#local-checks-before-push).
- Review as for any change, locally: a bug fix gets the single-agent check of
  [agent-code-review](../agent-code-review/SKILL.md#the-review-loop), and a PR that adds a protocol
  version gets the multi-agent loop. Reviewers read only the local diff, and nothing is posted.
- Protocol versions and reference-session proposal ids are assigned by the coordinator, as for
  public versions.
- Commit and title text follow [git-workflow](../../rules/git-workflow.md), and the PR body the
  squashed-commit format of [agent-create-pr](../agent-create-pr/SKILL.md#body). Neither names the
  finding's impact beyond what the advisory has already published.

## Landing a fix

Only the coordinator session lands a fix.

1. Rebase the fix on current `main`, renumber its protocol version if needed, and squash it to one
   commit.
2. Push that exact commit to the advisory fork's PR branch and to the private CI mirror
   `monkey-trading/engine-security` (remote `private-ci`). The mirror's `main` is upstream `main`
   plus one commit that drops the nightly schedule.
3. Run `gh workflow run ci.yaml -R monkey-trading/engine-security --ref <branch>`.
4. When `ci-passed` is green on that exact head, report the 40-character head and the advisory
   link to the owner.
5. The owner presses "Merge pull request(s)" on the advisory. That merge skips branch rules,
   required checks and the merge queue, so the mirror's green run is the check and the owner's
   press is the approval. Never post `merge-approved` or queue a PR for a finding.
6. Land a private protocol version only while no other version PR is queued.
7. CI on `main` after the push is a backstop; read it and report a failure.

## After the merge

- Delete the branch from the mirror, then remove the worktree and the `private-xxxx` remote.
- Keep the advisory in draft.
- Before launch, publish every advisory together at launch, with the first release and the audit
  report. Publishing deletes each temporary fork. Request no CVEs for pre-launch code.
- After launch, publish an advisory with the fix's release, as [SECURITY.md](../../../SECURITY.md)
  describes.

## Subagents

- A subagent never merges, publishes or creates an advisory, and never writes a finding to GitHub.
- It works in its own worktree, pushes only to the advisory's private fork, and reports the private
  PR's number and head to the coordinator.
