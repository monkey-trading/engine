#!/usr/bin/env bash
# The release's gate, run by .github/workflows/release-build.yaml in a checkout of the commit to build,
# with every branch and tag fetched. With TAG set, it refuses, exiting 1, unless TAG is a `v` followed by
# MAJOR.MINOR.PATCH, names the checked-out commit, and that commit is on origin/main with a passing
# ci.yaml run that ran every job: a push to main, the nightly run or a manual run. With TAG empty, a dry
# run, it reports each of those failures as a warning and goes on. It writes `sha` and `name`, the
# release's name or `dry-run-<commit>`, to $GITHUB_OUTPUT, and the release it names to
# $GITHUB_STEP_SUMMARY when that is set. scripts/release-test.sh tests it.
#
# Needs git, jq and gh with GH_REPO and a token that reads the repository's workflow runs.
#
# Usage: TAG=<tag or empty> scripts/release-gate.sh
set -euo pipefail

TAG=${TAG:-}
sha=$(git rev-parse --verify 'HEAD^{commit}')
refuse() {
  if [[ -n "$TAG" ]]; then
    echo "::error::$1"
    exit 1
  fi
  echo "::warning::$1; a tag would be refused"
}

if [[ -n "$TAG" ]]; then
  [[ "$TAG" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
    refuse "$TAG is not a v followed by MAJOR.MINOR.PATCH"
  [[ "$(git rev-parse --verify --quiet "refs/tags/$TAG^{commit}")" == "$sha" ]] ||
    refuse "$TAG does not name the commit $sha"
  name=$TAG
else
  name="dry-run-${sha:0:12}"
fi
git merge-base --is-ancestor "$sha" origin/main || refuse "$sha is not a commit of main"
# A pull request's or a merge group's run skips the jobs its changes do not reach.
passed=$(gh api "repos/$GH_REPO/actions/workflows/ci.yaml/runs?head_sha=$sha&status=success&per_page=100" |
  jq '[.workflow_runs[] | select(.event == "push" or .event == "schedule" or .event == "workflow_dispatch")] | length')
((passed > 0)) || refuse "no ci.yaml run that ran every job passed on $sha"

echo "sha=$sha" >> "$GITHUB_OUTPUT"
echo "name=$name" >> "$GITHUB_OUTPUT"
echo "Release $name from $sha" | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
