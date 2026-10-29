#!/usr/bin/env bash
# Checks check-merge-approved.sh against a stubbed GitHub API: one- and three-entry groups, a group
# whose first entry already merged, a status from another account, a revoked approval, a missing
# status, a group the queue does not hold, a ref naming another pull request or branch, and a base
# off the branch or ahead of it. Prints one line per case and exits 1 if any case fails.
set -euo pipefail
script="$(cd "$(dirname "$0")" && pwd)/check-merge-approved.sh"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
export FIXTURES="$tmp/fixtures" PATH="$tmp/bin:$PATH"
mkdir "$tmp/bin" "$FIXTURES"

# Answers the gate's three reads: the queue, the compare status of a commit against the branch, and
# a commit's statuses, newest first.
cat > "$tmp/bin/gh" <<'EOF'
#!/usr/bin/env bash
for arg; do
  case $arg in
    graphql) exec cat "$FIXTURES/queue.json" ;;
    */compare/*) printf '{"status":"%s"}\n' "$(cat "$FIXTURES/compare-${arg##*...}")"; exit ;;
    */statuses*) sha=${arg%/statuses*}; exec cat "$FIXTURES/statuses-${sha##*/}" ;;
  esac
done
exit 1
EOF
chmod +x "$tmp/bin/gh"

sha() { printf '%040x' "$1"; }
ref() { echo "refs/heads/gh-readonly-queue/$1/pr-$2-$3"; } # branch pr base
entry() { # pr base head pr-head
  printf '{"baseCommit":{"oid":"%s"},"headCommit":%s,"pullRequest":{"number":%s,"headRefOid":"%s"}}' \
    "$2" "$([ "$3" = null ] && echo null || printf '{"oid":"%s"}' "$3")" "$1" "$4"
}
queue() { local IFS=,; echo "{\"data\":{\"repository\":{\"mergeQueue\":{\"entries\":{\"nodes\":[$*]}}}}}" > "$FIXTURES/queue.json"; }
compare() { echo "$2" > "$FIXTURES/compare-$1"; } # commit status
statuses() { # sha state:creator..., newest first
  local sha=$1 list="" status; shift
  for status; do
    list+="${list:+,}{\"context\":\"merge-approved\",\"state\":\"${status%%:*}\",\"creator\":{\"login\":\"${status#*:}\"}}"
  done
  echo "[{\"context\":\"ci-passed\",\"state\":\"success\",\"creator\":{\"login\":\"github-actions[bot]\"}}${list:+,$list}]" \
    > "$FIXTURES/statuses-$sha"
}

fail=0
check() { # name want head-ref group-sha expected-output
  local out got=pass
  out=$(REPO=org/repo BASE_REF=refs/heads/main HEAD_REF=$3 GROUP_SHA=$4 APPROVER=owner "$script" 2>&1) || got=fail
  if [[ $got == "$2" ]] && grep -qF -- "$5" <<<"$out"; then
    echo "ok   $1"
  else
    echo "FAIL $1: $got, wanted $2 with '$5'"; echo "     ${out//$'\n'/$'\n'     }"; fail=1
  fi
}

main=$(sha 1) c1=$(sha 11) c2=$(sha 12) c3=$(sha 13) c4=$(sha 14)
p1=$(sha 21) p2=$(sha 22) p3=$(sha 23) p4=$(sha 24)
compare "$main" identical
compare "$c1" behind
statuses "$p1" success:owner
statuses "$p2" success:owner failure:owner
statuses "$p3" success:owner
statuses "$p4"

queue "$(entry 101 "$main" "$c1" "$p1")"
check "one approved pull request" pass "$(ref main 101 "$main")" "$c1" "#101 at $p1: merge-approved, success from owner."

# Entries behind the group, built or not, are no part of it.
everyone=("$(entry 101 "$main" "$c1" "$p1")" "$(entry 102 "$c1" "$c2" "$p2")" "$(entry 103 "$c2" "$c3" "$p3")"
  "$(entry 104 "$c3" "$c4" "$p4")" "$(entry 105 "$c4" null "$(sha 25)")")
queue "${everyone[@]}"
check "three approved pull requests, from the head down" pass "$(ref main 103 "$c2")" "$c3" "#101 at $p1"
check "an entry behind the group needs no approval" pass "$(ref main 102 "$c1")" "$c2" "#102 at $p2"
check "an unapproved entry behind the group fails its own group" fail "$(ref main 104 "$c3")" "$c4" \
  "#104 at $p4: the latest merge-approved status is missing"

queue "$(entry 102 "$c1" "$c2" "$p2")" "$(entry 103 "$c2" "$c3" "$p3")"
check "a group whose first entry merged stops at the branch" pass "$(ref main 103 "$c2")" "$c3" "#102 at $p2"

queue "${everyone[@]}"
statuses "$p2" success:other success:owner
check "a later status from another account" fail "$(ref main 103 "$c2")" "$c3" "#102 at $p2: the latest merge-approved status is success from other"
statuses "$p2" failure:owner success:owner
check "an approval the owner revoked" fail "$(ref main 103 "$c2")" "$c3" "is failure from owner"
statuses "$p2"
check "a head without merge-approved" fail "$(ref main 103 "$c2")" "$c3" "#102 at $p2: the latest merge-approved status is missing"
statuses "$p2" success:owner

check "a ref naming another pull request" fail "$(ref main 999 "$c2")" "$c3" "no entry for #999"
check "a ref on another branch" fail "$(ref dev 103 "$c2")" "$c3" "is not a merge queue ref for main"
check "a ref outside the queue" fail refs/heads/main "$c3" "is not a merge queue ref for main"
check "a group the queue does not hold" fail "$(ref main 103 "$c2")" "$(sha 99)" "no entry for #103"
echo '{"data":{"repository":{"mergeQueue":null}}}' > "$FIXTURES/queue.json"
check "no merge queue" fail "$(ref main 103 "$c2")" "$c3" "no entry for #103"

# An entry inside the group left the queue: the chain stops at a commit ahead of the branch.
queue "$(entry 101 "$main" "$c1" "$p1")" "$(entry 103 "$c2" "$c3" "$p3")"
compare "$c2" ahead
check "a group whose middle entry left the queue" fail "$(ref main 103 "$c2")" "$c3" "is not on main (compare: ahead)"

queue "${everyone[@]}"
compare "$main" diverged
check "a base off the branch" fail "$(ref main 103 "$c2")" "$c3" "is not on main (compare: diverged)"

exit "$fail"
