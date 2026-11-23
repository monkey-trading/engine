---
name: agent-security-fix
description: >
  Use when fixing, landing or recording a confirmed security finding — anything a security audit
  or a private report found that must not be public before its fix lands. Covers recording a
  finding as a draft repository security advisory, keeping every artefact private until the fix is
  merged, building the fix in the advisory's temporary private fork, and landing it through the
  public merge queue: before launch, once the fix is ready; after launch, once its release is live,
  with the system contained first and the release approved by the timelock, and disclosing after.
  Does not cover ordinary bug PRs (agent-create-pr) or reviewing a public PR.
---

# agent-security-fix

Fix and land a confirmed security finding without publishing it before its fix is live. The
repository is public, so everything about an unpublished finding stays in the working record, the
advisory and its private fork. Ordinary PRs go through [agent-create-pr](../agent-create-pr/SKILL.md);
this skill replaces its branch, push and review-posting steps for a finding, and keeps its
local review, check and merge rules. [Before launch](#before-launch) no funds are exposed and the fix
lands on `main` first. [After launch](#after-launch) live funds are exposed while the fix is in
progress, so the system is contained first and the fix ships as a release, and lands on `main`
once that release is live.

## How a fix lands

A fix lands through the public merge queue, with full CI and the owner's `merge-approved`, like any
PR. It never lands through the advisory's "Merge pull request(s)" button, a ruleset bypass or
`gh pr merge --admin`.

Merging into a public repository is the disclosure: the diff, tests and commit message reveal the
flaw. Concealment therefore comes only from when the fix is merged, not how. The advisory's private
fork keeps the finding, the exploit tests, the review and the fix out of sight until that moment.

## Rules for both phases

### Recording a finding

- Reports arrive through GitHub's private vulnerability reporting, the primary channel, or the email
  address in [SECURITY.md](../../../SECURITY.md#reporting-a-vulnerability). The coordinator reads
  them; no PGP key is published.
- A finding becomes an advisory only after it has been reproduced independently, outside the
  session that found it.
- Each finding is one draft repository security advisory:
  `POST /repos/{owner}/{repo}/security-advisories`, with a summary, a description, a severity and
  the affected component. Grade severity with the
  [severity guide](../agent-code-review/references/severity.md).
- The working record (reports, exploit tests, the findings ledger) lives only in the main
  checkout's gitignored `agents/.processes/` tree, never in a tracked file.

### Confidentiality

- Never push a fix branch to `origin`, and never open its public PR, before the moment the phase
  below names: before launch, when the fix is ready to land; after launch, once the fixed release
  is live.
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

### Who acts

- Subagents never merge, publish, deploy, pause, create an advisory, write a finding to GitHub or
  contact a reporter.
- A subagent works in its own worktree, pushes only to the advisory's private fork, and reports the
  private PR's number and head to the coordinator.
- The coordinator creates advisories, lands fixes and proposes every action on a live system. The
  owner decides and acts on a live system, as [no-deploy](../../rules/no-deploy.md) requires: an
  agent writes the exact command and waits.

### Building the fix

- Work in the advisory's temporary private fork, created with
  `POST /repos/{owner}/{repo}/security-advisories/{ghsa}/forks`.
- **Only one open PR per advisory may target `main`.** GitHub blocks the advisory's merge
  otherwise. Related advisories fixed together live in one advisory's fork. A follow-up fix to the
  same advisory opens its PR only after the first has merged.
- The exploit tests become regression tests, named by the behaviour they hold, failing before the
  fix and passing after it.
- Write one mutation per fix and revert it afterwards.
- A temporary fork runs no CI. Run locally what CI would run for the touched areas, per
  [git-workflow](../../rules/git-workflow.md#local-checks-before-push), and use the
  [private CI mirror](#the-private-ci-mirror) for a full run.
- Review as for any change, locally: a bug fix gets the single-agent check of
  [agent-code-review](../agent-code-review/SKILL.md#the-review-loop), and a PR that adds a protocol
  version gets the multi-agent loop. Reviewers read only the local diff, and nothing is posted.
- Protocol versions and reference-session proposal ids are assigned by the coordinator, as for
  public versions.
- Commit text follows [git-workflow](../../rules/git-workflow.md) and the PR body the
  squashed-commit format of [agent-create-pr](../agent-create-pr/SKILL.md#body), with no more
  about the finding than its advisory carries.

## Before launch

### The private CI mirror

`monkey-trading/engine-security` (remote `private-ci`) runs the repository's CI without publishing
anything. Its `main` is upstream `main` plus one commit that drops the nightly schedule. Start a run
with `gh workflow run ci.yaml -R monkey-trading/engine-security --ref <branch>`.

The mirror runs on 2-core runners, while public CI runs on 4-core ones. In the mirror these timing
checks are informational:

- `price_wait_budget`;
- the command-latency and launch-book budgets;
- the stats cost.

Every correctness job must be green there, and no correctness step may be skipped behind a failed
timing step.

### Landing a fix

Only the coordinator session lands a fix. When a fix is ready, the coordinator:

1. rebases it on current `main`, renumbers its protocol version if needed, and squashes it to one
   commit;
2. optionally runs the private CI mirror on that commit;
3. pushes that exact commit to a public branch and opens a normal PR;
4. queues it through the merge queue with full CI and the owner's `merge-approved`, as
   [agent-create-pr](../agent-create-pr/SKILL.md#merging) describes for any PR.

Nothing is deployed before launch, so the public window between the push and the merge harms no
one. Land a protocol version only while no other version PR is queued. The advisory stays in draft.

### After the merge

- Close the advisory fork's PR.
- Delete the mirror's branch, then remove the worktree and the `private-xxxx` remote.
- Once the advisory's last fix has landed and its fork has no open PR, delete the fork with
  `gh repo delete monkey-trading/<fork> --yes`, which needs the token's `delete_repo` scope. Record
  the landing in the working record first: the landed commits and the record keep everything the
  fork held. A fork whose advisory still has a follow-up fix to land stays until that fix lands.
- Keep the advisory in draft.
- Publish every advisory together at launch, with the first release and the audit report.
  Publishing deletes any temporary fork still left. Request no CVEs for pre-launch code.

## After launch

The engine holds live funds, so a finding is a live exposure until its fix is live. The
coordinator proposes each step; the owner decides and acts.

1. **Triage first.** Follow [SECURITY.md](../../../SECURITY.md#what-happens-next) for the
   response times: acknowledge a private report, tell the reporter whether it reproduces, how
   severe it is and what is planned, and keep the reporter informed until it is resolved. Grade
   severity with the severity guide. Record the finding as above once it reproduces
   independently.
2. **Contain by severity, before fixing.** The coordinator proposes; the owner acts.
   - **Critical:** the guardian's `pauseWithdrawals()` and every affected market reduce-only, at
     once.
   - **High:** the affected markets reduce-only. Add the withdrawal pause only if funds can leave
     through the flaw.
   - **Medium and Low:** no containment. The fix goes at the normal pace.

   The levers and their runbooks are in
   [incident-response](../../../ops/runbooks/incident-response.md#the-ladder-of-containment):
   [reduce-only](../../../ops/runbooks/reduce-only.md), the guardian's pause (seven days, which the
   guardian cannot extend, so the coordinator notes its expiry), the timelock's `setPauses` for
   deposits or withdrawals, and the stress floor (which lapses after 72 hours). While votes
   govern, a fast-path restriction by vote of the rotation set
   ([operator-candidate](../../../ops/runbooks/operator-candidate.md)) is one more. The permanent
   exit ([restart-and-recovery](../../../ops/runbooks/restart-and-recovery.md)) is the fallback
   when the vault itself cannot be made safe.

   Containment holds for the whole window until the fix is live, including the timelock's delay.
   It is lifted when the fixed release is the vault's current release, or when a contract
   migration is complete.
3. **Fix privately,** as in [Building the fix](#building-the-fix): the advisory's fork, the same
   review level, and the [private CI mirror](#the-private-ci-mirror) as the pre-release CI, because
   nothing may be public before activation.
4. **Ship an engine or node fix as a release,** in this order. The fix's source stays private until
   the fixed release activates.
   1. Build and test the fix privately: the advisory's fork and the mirror, with every correctness
      job green. Do not push it to `origin` or merge it.
   2. Build the reproducible image from that exact private commit and write the release manifest, as
      in [deploy-and-rollback](../../../ops/runbooks/deploy-and-rollback.md#prepare-a-release) and
      [image/README.md](../../../image/README.md#release).
   3. The owner proposes the release to the timelock (`setCurrentRelease`). At the proposal, the
      owner adds each rotation-set member's maintainer as a collaborator on the draft advisory.
      That gives them its private fork to build the image and verify the release's measurement
      themselves, under embargo until activation.
   4. Containment holds through the timelock's delay. The release activates and the node upgrades
      by handover ([deploy-and-rollback](../../../ops/runbooks/deploy-and-rollback.md#upgrade-to-a-new-release)).
   5. At activation, push the release's exact commit as a public tag, so anyone can reproduce the
      release's measurement and verify its attestation. That moment, not the timelock proposal, is
      when the source becomes public.
   6. Merge the fix through the public merge queue, as in [How a fix lands](#how-a-fix-lands). The
      release is live by then, so the fix's time in the queue exposes nothing.
   7. Then publish the advisory, as step 7 says.
5. **A contract finding has no hot fix.** The deployed contracts cannot be patched in place.
   1. Pause what the finding reaches, through the guardian or the timelock.
   2. Deploy a fixed contract through the timelock.
   3. Move clients over by withdrawals and re-deposits, with the old contract kept withdraw-only.

   If the vault itself cannot be made safe, the permanent exit is the fallback. Never describe a
   contract fix as a patch to the live contract.
6. **Tell users when containment affects them.** Announce it at once, in neutral wording such as
   "security maintenance", on the project's announcement channel and in the console. Give full
   details at publication.
7. **Disclose** every advisory, of any severity, once the release containing its fix is the vault's
   current release and the fix has merged, as [SECURITY.md](../../../SECURITY.md#coordinated-disclosure) says.
   - Request a CVE for Critical and High findings.
   - Credit the reporter if they wish.
   - Bounty payouts follow the scale in [SECURITY.md](../../../SECURITY.md#bug-bounty). It is a launch blocker (#72).
8. **Deadline.** If a fix cannot meet `SECURITY.md`'s deadline, agree one extension with the
   reporter in writing, of at most 30 days, and keep containment in place. Publish when the fix is
   live or at the extended deadline, whichever comes first.
9. **Afterwards,** do the [post-incident steps](../../../ops/runbooks/incident-response.md#after-the-incident),
   then re-run the relevant audit area on the fixed release. Close the fork's PR, delete the
   mirror's branch, the worktree and the `private-xxxx` remote, and keep the advisory in draft
   until it is published.

## Decisions (owner, 2026-11-17)

These settle what the repository's records left open. Each also stands in the step it governs.

- **Containment** follows the finding's severity, and is lifted when the fixed release is current
  or a contract migration is complete (After launch, step 2).
- **Landing** is through the public merge queue, never the advisory's merge button, a ruleset
  bypass or `gh pr merge --admin`: the merge is the disclosure, so only its timing conceals
  (How a fix lands).
- **Engine and node fixes** stay private until the fixed release activates, with containment
  through the timelock's delay and each rotation-set member's maintainer added to the draft
  advisory at the proposal; the release's commit is tagged publicly at activation and the fix then
  merges through the queue (step 4).
- **Contract findings** are paused, redeployed through the timelock and migrated by withdrawals
  and re-deposits, with the permanent exit as the fallback (step 5).
- **Users** are told at once, in neutral wording, when containment affects them (step 6).
- **Disclosure** covers every advisory once its fix's release is current; a CVE is requested for
  Critical and High; the bounty follows #72's scale (step 7).
- **Reporting** goes through GitHub's private vulnerability reporting or the email in
  `SECURITY.md`, with no PGP key.
- **Deadline:** one written extension of at most 30 days, with containment kept (step 8).
