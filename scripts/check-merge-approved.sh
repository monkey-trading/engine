#!/usr/bin/env bash
# Pass a merge group only if every pull request in it has the owner's go-ahead: the latest
# `merge-approved` commit status on the pull request's current head is `success` and was posted by
# $APPROVER. `.github/workflows/merge-approval.yaml` runs it on every merge group.
#
# Env: GH_TOKEN; REPO (owner/name); BASE_REF, HEAD_REF, and GROUP_SHA, the merge group's target
# branch, ref, and head commit; APPROVER, the owner's GitHub login.

set -euo pipefail

: "${REPO:?}" "${BASE_REF:?}" "${HEAD_REF:?}" "${GROUP_SHA:?}" "${APPROVER:?}"
branch=${BASE_REF#refs/heads/}

# The queue names a group's ref after the pull request it was built for.
if [[ ! $HEAD_REF =~ ^refs/heads/gh-readonly-queue/$branch/pr-([0-9]+)-[0-9a-f]{40}$ ]]; then
  echo "::error::$HEAD_REF is not a merge queue ref for $branch."
  exit 1
fi
group_pr=${BASH_REMATCH[1]}

# shellcheck disable=SC2016 # GraphQL variables, not shell ones.
queue=$(gh api graphql -f owner="${REPO%/*}" -f name="${REPO#*/}" -f branch="$branch" -f query='
  query($owner: String!, $name: String!, $branch: String!) {
    repository(owner: $owner, name: $name) {
      mergeQueue(branch: $branch) {
        entries(first: 100) {
          nodes { baseCommit { oid } headCommit { oid } pullRequest { number headRefOid } }
        }
      }
    }
  }')

# Each queue entry's commit sits on the entry ahead of it, so following base commits down from the
# group's head names every pull request in the group, and stops at the first commit that is no
# entry's.
chain=$(jq -c --arg head "$GROUP_SHA" '
  (.data.repository.mergeQueue.entries.nodes // []
    | map(select(.headCommit != null) | {key: .headCommit.oid, value: .}) | from_entries) as $entries
  | [$head | recurse($entries[.].baseCommit.oid // empty)] as $commits
  | {root: $commits[-1], prs: [$commits[:-1][] | $entries[.].pullRequest]}' <<<"$queue")

if [[ $(jq -r '.prs[0].number // empty' <<<"$chain") != "$group_pr" ]]; then
  echo "::error::The merge queue has no entry for #$group_pr at $GROUP_SHA."
  exit 1
fi

# The group may hold nothing but its entries' commits on top of the branch.
root=$(jq -r .root <<<"$chain")
on_branch=$(gh api "repos/$REPO/compare/$branch...$root" | jq -r .status)
if [[ $on_branch != identical && $on_branch != behind ]]; then
  echo "::error::$root, under the group's queue entries, is not on $branch (compare: $on_branch)."
  exit 1
fi

failed=0
for pr in $(jq -r '.prs[] | "\(.number):\(.headRefOid)"' <<<"$chain"); do
  number=${pr%%:*} head=${pr#*:}
  # Statuses list newest first; the latest one per context is the one branch rules read.
  latest=$(gh api --paginate "repos/$REPO/commits/$head/statuses?per_page=100" | jq -rs '
    add // [] | map(select(.context == "merge-approved")) | first
    | if . == null then "missing" else "\(.state) from \(.creator.login)" end')
  if [[ $latest == "success from $APPROVER" ]]; then
    echo "#$number at $head: merge-approved, $latest."
  else
    echo "::error::#$number at $head: the latest merge-approved status is $latest, not success from $APPROVER."
    failed=1
  fi
done
exit "$failed"
