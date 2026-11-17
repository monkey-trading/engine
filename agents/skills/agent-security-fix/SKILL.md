---
name: agent-security-fix
description: >
  Use when fixing, landing or recording a confirmed security finding — anything a security audit
  or a private report found that must not be public before its fix lands. Covers recording a
  finding as a draft repository security advisory, keeping every artefact private, and then, before
  launch, building the fix in the advisory's temporary private fork and landing it through the
  private CI mirror on the owner's merge; and, after launch, containing the live system first,
  shipping the fix as a release the timelock approves, and disclosing once the release is live.
  Does not cover ordinary bug PRs (agent-create-pr) or reviewing a public PR.
---

# agent-security-fix

Fix and land a confirmed security finding without publishing it before its fix is live. The
repository is public, so everything about an unpublished finding stays in the working record, the
advisory and its private fork. Ordinary PRs go through [agent-create-pr](../agent-create-pr/SKILL.md);
this skill replaces its branch, push, review-posting and merge steps for a finding, and keeps its
local review and check rules. [Before launch](#before-launch) no funds are exposed and the fix
lands on `main` first. [After launch](#after-launch) live funds are exposed while the fix is in
progress, so the system is contained first and the fix ships as a release.

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
  [git-workflow](../../rules/git-workflow.md#local-checks-before-push).
- Review as for any change, locally: a bug fix gets the single-agent check of
  [agent-code-review](../agent-code-review/SKILL.md#the-review-loop), and a PR that adds a protocol
  version gets the multi-agent loop. Reviewers read only the local diff, and nothing is posted.
- Protocol versions and reference-session proposal ids are assigned by the coordinator, as for
  public versions.
- Commit text follows [git-workflow](../../rules/git-workflow.md) and the PR body the
  squashed-commit format of [agent-create-pr](../agent-create-pr/SKILL.md#body), with no more
  about the finding than its advisory carries.

## Before launch

### Landing a fix

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

### After the merge

- Delete the branch from the mirror, then remove the worktree and the `private-xxxx` remote.
- Keep the advisory in draft.
- Publish every advisory together at launch, with the first release and the audit report.
  Publishing deletes each temporary fork. Request no CVEs for pre-launch code.

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
3. **Fix privately,** as in [Building the fix](#building-the-fix): the advisory's fork, the CI
   mirror for the green run, and the same review level.
4. **Ship an engine or node fix as a release,** in this order. The fix's source stays private until
   the fixed release activates.
   1. Verify the fix in the advisory's private fork and on the CI mirror, as in
      [Landing a fix](#landing-a-fix), but do not merge it.
   2. Build the reproducible image from the fix commit and write the release manifest, as in
      [deploy-and-rollback](../../../ops/runbooks/deploy-and-rollback.md) section 1 and
      [image/README.md](../../../image/README.md#release).
   3. The owner proposes the release to the timelock (`setCurrentRelease`). At the proposal, the
      owner adds each rotation-set member's maintainer as a collaborator on the draft advisory.
      That gives them its private fork to build the image and verify the release's measurement
      themselves, under embargo until activation.
   4. Containment holds through the timelock's delay. The release activates and the node upgrades
      by handover ([deploy-and-rollback](../../../ops/runbooks/deploy-and-rollback.md) section 3).
   5. The advisory's merge puts the fix on `main` at activation. That moment, not the timelock
      proposal, is when the source becomes public, so anyone can then reproduce the release's
      measurement from public source and verify its attestation.
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
   current release, as [SECURITY.md](../../../SECURITY.md#coordinated-disclosure) says.
   - Request a CVE for Critical and High findings.
   - Credit the reporter if they wish.
   - Bounty payouts follow the scale #72 publishes. #72 is a launch blocker.
8. **Deadline.** If a fix cannot meet `SECURITY.md`'s deadline, agree one extension with the
   reporter in writing, of at most 30 days, and keep containment in place. Publish when the fix is
   live or at the extended deadline, whichever comes first.
9. **Afterwards,** do the [post-incident steps](../../../ops/runbooks/incident-response.md#after-the-incident),
   then re-run the relevant audit area on the fixed release. Delete the mirror's branch, the
   worktree and the `private-xxxx` remote.

## Decisions (owner, 2026-11-17)

These settle what the repository's records left open. Each also stands in the step it governs.

- **Containment** follows the finding's severity, and is lifted when the fixed release is current
  or a contract migration is complete (After launch, step 2).
- **Engine and node fixes** stay private until the fixed release activates, with containment
  through the timelock's delay and each rotation-set member's maintainer added to the draft
  advisory at the proposal (step 4).
- **Contract findings** are paused, redeployed through the timelock and migrated by withdrawals
  and re-deposits, with the permanent exit as the fallback (step 5).
- **Users** are told at once, in neutral wording, when containment affects them (step 6).
- **Disclosure** covers every advisory once its fix's release is current; a CVE is requested for
  Critical and High; the bounty follows #72's scale (step 7).
- **Reporting** goes through GitHub's private vulnerability reporting or the email in
  `SECURITY.md`, with no PGP key.
- **Deadline:** one written extension of at most 30 days, with containment kept (step 8).
