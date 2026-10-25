---
name: agent-build-feature
description: >
  Use when implementing a feature — "build #12", "implement this feature", "start the next
  feature". Confirms the feature is ready to build, builds all of it on one branch for one PR
  (parallel subagents allowed), self-reviews it with agent-code-review, runs its dev scenario on
  the branch before the PR is marked ready, and opens the PR through agent-create-pr ending
  `Closes #<feature>`, so the merge closes the issue. Deploys and issue writes need the user's
  approval, except the `bug` issues the review loop files for Mediums from round 3 on.
---

# agent-build-feature

Build one ready feature end to end on one branch, so the team reviews it once, tests it in dev, and
promotes it to prod. Features are planned with [agent-plan-feature](../agent-plan-feature/SKILL.md);
a bug fix or a refactor that is not a feature goes straight through
[git-workflow](../../rules/git-workflow.md) and [agent-create-pr](../agent-create-pr/SKILL.md).

## Before the first edit

- **Ready.** Read the feature issue (`gh issue view <n> --comments`). A planned feature carries the `ready-to-build`
  label, every blocker is done, and its acceptance criteria cite their sources. If a decision is
  open, stop and name it. Build nothing of a feature that is not ready, not even the parts that look
  settled.
- **An issue the plan did not write.** Check the same things: every decision its design needs is
  made, its criteria have sources, and it has a dev scenario. Name each gap and ask the user to fill
  it or confirm the issue is ready; never decide that alone.
- **Whole scope.** List what the feature needs to run in dev: protocol rules, log and state-root
  compatibility, adapters, API, config and secrets, contracts, monitoring. A backend feature is
  complete when its dev scenario runs through its API, an operator command, or a scripted check;
  app work is its own feature. Anything the issue does not cover is a question for the user now, not a later PR.
- **Size.** Estimate the PR from that scope, counting production code and tests as
  [agent-plan-feature](../agent-plan-feature/SKILL.md#what-a-feature-is) does, and again when
  planning the build (the interfaces the subagents build against) shows more than the issue
  suggested. When the issue records why a large feature has no seam and the estimate has not grown
  past its size, build it. Otherwise, past about 5k lines, look for a seam as agent-plan-feature
  describes and propose any split to the user before the first edit; past about 10k, build one PR
  only when they agree there is no seam. An agreed split changes the plan: re-plan through
  agent-plan-feature, then build the first resulting feature once its issue is ready.
- **Branch.** One pushed branch for the feature, named for its issue per git-workflow.
- Load [think-before-coding](../../rules/think-before-coding.md),
  [development-checklist](../../rules/development-checklist.md), and the `*-implementation` skill
  for every area the feature touches.

## Building

- Build the narrowest end-to-end path first, then widen it, so the branch runs at every step.
- Parallel work is fine: split by area across subagents, each in its own worktree on a local
  branch, and merge each into the feature branch. Never push a branch or open a PR per part, and
  never stack PRs for one feature.
- Map every acceptance criterion to code and a test. Test where the behaviour lives: unit tests for
  rules (`engine-core` behaviour goes through `Engine::apply`), integration tests at the chain,
  feed, and enclave boundaries, and a scripted check that runs the feature's dev scenario.

## Before review

- Run the local checks per git-workflow.
- Self-review with [agent-code-review](../agent-code-review/SKILL.md), multi-agent at `medium` or
  higher: fix every accepted Critical, High, and Medium finding by class
  ([Fixing an accepted finding](../agent-code-review/SKILL.md#fixing-an-accepted-finding)) and run
  it again until [the review loop](../agent-code-review/SKILL.md#the-review-loop) ends, as
  [agent-create-pr](../agent-create-pr/SKILL.md#before-a-pr-is-ready-for-review) requires before
  the PR is marked ready.
- Show the user the dev scenario — steps, expected result, and the config it needs — and on their
  yes write it into the issue's dev scenario section, replacing the planned text.
- Rebase the branch onto `main` and run the dev scenario in dev on that build: a local node built
  from the branch, or a hosted dev environment once one exists, deployed only on the user's
  explicit approval ([no-deploy](../../rules/no-deploy.md)). Show the user the result. A
  mechanical rebase needs no new review round; one that changes behaviour does
  ([the review loop](../agent-code-review/SKILL.md#the-review-loop)).
- The scenario's scripted check passes in CI.
- Put what promotion needs in the PR body: config, secrets, genesis or parameter changes, contract
  deployments, deploy ordering, rollback.

## PR and merge

- Open the PR with agent-create-pr, ending `Closes #<feature>`, never `Part of #<feature>`. The PR
  is marked ready only after the dev scenario has passed on the rebased branch and its scripted
  check is green in CI, so the merge finishes the feature and GitHub closes the issue.
- Answer review with [agent-address-pr-comments](../agent-address-pr-comments/SKILL.md), aiming for
  one round.
- A later push that changes the feature's behaviour, from a review fix or a rebase, runs the dev
  scenario again on the new head and shows the user the result before the PR merges.
- Promotion to prod needs its own approval.

## Rules

- One feature never spans several pushed branches or PRs.
- Never build part of a feature that is not ready.
- Never write to a GitHub issue without the user's yes to the exact text. The exceptions are the
  merge closing the feature issue through `Closes #<feature>`, and the `bug` issues
  [the review loop](../agent-code-review/SKILL.md#the-review-loop) files for Mediums from round 3
  on.
- Never deploy or change an environment without explicit per-action approval.
