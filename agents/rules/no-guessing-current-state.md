# No guessing current state

Load on request when a task depends on state that may have changed since the last session.

**Rule**: Verify live state before acting or claiming.

- Check GitHub PRs, CI, reviews, branches, deploys, packages, contract deployments, model catalogs, and external docs from their current source.
- Use local git / repo files for local state; use GitHub, CI, the chain, and a running node's API for remote state.
- Quote the command, source, PR, commit, run, or file that established the state.
- If current state cannot be checked, say what is unverified and avoid merge / deploy / release claims.
- Memory can suggest where to look; it is not proof of current state.

**A green check is not the same as "everything ran".** Before reporting CI green, know which jobs
actually ran on that commit:

- A **cancelled** job is not a passed job. On a pull request, CI cancels the whole run as soon
  as one job fails, so "only job X failed" usually means the others never finished — check every
  job's conclusion, not just the failures.
- After a flake, `gh run rerun --failed` re-runs the failed, cancelled, and unstarted jobs, not
  only the flaky one; read the new attempt's `ci-passed`, not the first attempt's.
- Verify a push produced a new run: `git push` on an unchanged branch succeeds silently and
  triggers nothing, which looks identical to a real push. Match the run's `headSha` to the branch
  tip before believing a result belongs to your change.

Why: stale state causes false approvals, wrong deploy claims, duplicate fixes, and broken follow-up work.
