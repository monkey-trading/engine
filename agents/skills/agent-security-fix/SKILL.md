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

The engine holds live funds, so a finding is a live exposure until the fixed release is the
vault's current release. The coordinator proposes each step; the owner decides and acts.

1. **Triage first.** Follow [SECURITY.md](../../../SECURITY.md#what-happens-next) for the
   response times: acknowledge a private report, tell the reporter whether it reproduces, how
   severe it is and what is planned, and keep the reporter informed until it is resolved. Grade
   severity with the severity guide. Record the finding as above once it reproduces
   independently.
2. **Contain before fixing, for a live Critical or High.** The coordinator proposes the steps; the
   owner decides and acts. Take the least drastic step that stops the harm, from
   [incident-response](../../../ops/runbooks/incident-response.md#the-ladder-of-containment):
   - make the affected markets reduce-only ([reduce-only](../../../ops/runbooks/reduce-only.md));
   - the guardian's `pauseWithdrawals()`, up to seven days, which the guardian cannot extend;
   - the timelock's `setPauses` to pause deposits, or withdrawals, until it lifts them;
   - the stress floor, which lapses after 72 hours unless set again;
   - while votes govern, a fast-path restriction by vote of the rotation set
     ([operator-candidate](../../../ops/runbooks/operator-candidate.md));
   - as a last resort, stopping the node, which starts the vault's exit deadlines
     ([restart-and-recovery](../../../ops/runbooks/restart-and-recovery.md)).

   Containment must hold for the whole window until the fixed release is live, so plan each
   lever's expiry and renewal against the schedule in step 4 and put each renewal in the
   coordinator's notes.
3. **Fix privately,** as in [Building the fix](#building-the-fix): the advisory's fork, the CI
   mirror for the green run, and the same review level.
4. **Ship an engine or node fix as a release,** in this order:
   1. Merge the fix to `main` through the advisory's merge, as in [Landing a fix](#landing-a-fix).
      It is public from then on.
   2. Rebuild the reproducible image from the fix commit and write the release manifest, as in
      [deploy-and-rollback](../../../ops/runbooks/deploy-and-rollback.md) section 1 and
      [image/README.md](../../../image/README.md#release). The release's measurement must
      reproduce from public source, so a verifier can check its attestation.
   3. The timelock approves the release with `setCurrentRelease`, and the node upgrades by
      handover ([deploy-and-rollback](../../../ops/runbooks/deploy-and-rollback.md) section 3).

   The fix is merged and public no later than the moment the release goes to the timelock, and
   the timelock's delay is the public window: containment holds through it.
5. **A contract finding has no hot fix.** The deployed contracts cannot be patched in place.
   Contain as in step 2, then the owner decides a migration or redeployment plan, through the
   timelock where it applies. Never describe a contract fix as a patch to the live contract.
6. **Disclose once the fixed release is the vault's current release and the deployment is done.**
   - Publish the advisory, as [SECURITY.md](../../../SECURITY.md#coordinated-disclosure) says, and
     credit the reporter by name unless they would rather not be.
   - Request a CVE for a Critical or High finding, on the owner's confirmation.
   - The owner directs the announcement to users through the project's public channels.
   - Bug bounty payouts follow the programme #72 sets, and the owner decides each one.
7. **Afterwards,** do the [post-incident steps](../../../ops/runbooks/incident-response.md#after-the-incident),
   then re-run the relevant audit area on the fixed release. Delete the mirror's branch, the
   worktree and the `private-xxxx` remote.

## Owner decisions

The repository's records do not settle these. Do not choose them; ask.

- Which containment steps to take for a given finding, and when to lift each.
- Whether a Critical or High finding's fix may wait for the timelock's delay with containment in
  place, or needs a different route.
- The migration or redeployment plan for a contract finding.
- Whether to request a CVE, finding by finding.
- The announcement's channels and wording.
- Each bug bounty payout, and its amount.
- The reporter's contact path before a PGP key is published in `SECURITY.md`, and who writes to
  the reporter.
- The advisory's fix date when the 90-day limit in `SECURITY.md` cannot be met.
