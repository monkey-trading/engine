#!/usr/bin/env bash
# Checks the release workflow's logic with `gh` stubbed: scripts/release-gate.sh on a scratch repository,
# a tag released only when it is a `v` and MAJOR.MINOR.PATCH naming the checked-out commit of main with a
# passing push, nightly or manual CI run, and a dry run warning instead of refusing; and release.yaml's
# step that creates the draft release, run as the workflow runs it, creating a draft, completing a
# partial one, and refusing to touch a published release. Prints one line per case and exits 1 if any
# fails. Needs git, jq and yq; touches nothing outside a temporary directory.
#
# Usage: scripts/release-test.sh
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
failed=0
pass() { echo "ok   $1"; }
fail() { echo "FAIL $1"; failed=1; }

# `gh api` answers only a query of ci.yaml's runs, from $RUNS/<sha>.json or with no runs, keeping those
# whose conclusion is the query's `status`; `gh release` acts on $RELEASE (none, draft or published).
# Every call is logged to $CALLS.
mkdir "$tmp/bin"
cat > "$tmp/bin/gh" << 'EOF'
#!/usr/bin/env bash
echo "$*" >> "$CALLS"
case "$1 $2" in
  "api "*)
    [[ "$2" == repos/*/actions/workflows/ci.yaml/runs\?* ]] || { echo "unexpected query: $2" >&2; exit 1; }
    sha=$(sed -n 's/.*[?&]head_sha=\([0-9a-f]*\).*/\1/p' <<< "$2")
    status=$(sed -n 's/.*[?&]status=\([a-z_]*\).*/\1/p' <<< "$2")
    runs=$(cat "$RUNS/$sha.json" 2> /dev/null || echo '{"workflow_runs": []}')
    jq --arg status "$status" '{workflow_runs: [.workflow_runs[] | select($status == "" or .conclusion == $status)]}' \
      <<< "$runs" ;;
  "release view")
    case "$RELEASE" in none) exit 1 ;; draft) echo true ;; published) echo false ;; esac ;;
esac
EOF
chmod +x "$tmp/bin/gh"
export CALLS="$tmp/calls" RUNS="$tmp/runs" PATH="$tmp/bin:$PATH" GH_REPO=owner/engine
mkdir "$RUNS"

# main: first, second (origin/main); a side branch's commit that main does not hold.
repo="$tmp/repo"
git init -q -b main "$repo"
commit() { git -C "$repo" -c user.name=t -c user.email=t@t commit -q --allow-empty -m "$1" && git -C "$repo" rev-parse HEAD; }
first=$(commit first)
second=$(commit second)
git -C "$repo" update-ref refs/remotes/origin/main "$second"
git -C "$repo" checkout -q -b side
side=$(commit side)
for tag in v0.1.0 v1.0.0 v0.2.0-rc1 v01.0.0 0.1.0; do git -C "$repo" tag "$tag" "$second"; done
git -C "$repo" tag v0.3.0 "$first"
git -C "$repo" tag v0.4.0 "$side"
# runs <sha> <event>:<conclusion>...
runs() {
  jq -n --args '{workflow_runs: [$ARGS.positional[] | split(":") | {event: .[0], conclusion: .[1]}]}' "${@:2}" \
    > "$RUNS/$1.json"
}
runs "$second" pull_request:success push:success
runs "$side" push:success

# gate <case> <commit> <tag> <expected exit> <expected output>
gate() {
  local name=$1 at=$2 tag=$3 want=$4 expect=$5 out="$tmp/output" log="$tmp/log" got=0
  : > "$out"
  git -C "$repo" checkout -q --detach "$at"
  (cd "$repo" && TAG=$tag GITHUB_OUTPUT=$out "$root/scripts/release-gate.sh") > "$log" 2>&1 || got=$?
  if [[ "$got" == "$want" ]] && grep -qF -- "$expect" "$log" "$out"; then pass "gate: $name"; else
    fail "gate: $name (exit $got, want $want and '$expect')"; sed 's/^/     /' "$log"; fi
}
gate "a tag on main with a push run" "$second" v0.1.0 0 "name=v0.1.0"
gate "the commit it releases" "$second" v0.1.0 0 "sha=$second"
gate "a later major version" "$second" v1.0.0 0 "name=v1.0.0"
gate "a pre-release" "$second" v0.2.0-rc1 1 "is not a v followed by MAJOR.MINOR.PATCH"
gate "a leading zero" "$second" v01.0.0 1 "is not a v followed by MAJOR.MINOR.PATCH"
gate "no v" "$second" 0.1.0 1 "is not a v followed by MAJOR.MINOR.PATCH"
gate "a tag naming another commit" "$second" v0.3.0 1 "v0.3.0 does not name the commit $second"
gate "a commit off main" "$side" v0.4.0 1 "$side is not a commit of main"
runs "$second" pull_request:success merge_group:success
gate "only pull request and merge group runs" "$second" v0.1.0 1 "no ci.yaml run that ran every job passed"
runs "$second" push:failure schedule:cancelled
gate "a failed push run" "$second" v0.1.0 1 "no ci.yaml run that ran every job passed"
runs "$second" schedule:success
gate "a nightly run" "$second" v0.1.0 0 "name=v0.1.0"
runs "$second" workflow_dispatch:success
gate "a manual run" "$second" v0.1.0 0 "name=v0.1.0"
rm "$RUNS/$second.json"
gate "no run at all" "$second" v0.1.0 1 "no ci.yaml run that ran every job passed"
gate "a dry run off main warns" "$side" "" 0 "::warning::$side is not a commit of main; a tag would be refused"
gate "a dry run with no run warns" "$second" "" 0 "::warning::no ci.yaml run that ran every job passed"
gate "a dry run's name" "$side" "" 0 "name=dry-run-${side:0:12}"

# The publish job's step, under the shell an unnamed `run` gets: bash -e.
step=$(yq '.jobs.publish.steps[] | select(.name == "Create the draft release") | .run' \
  "$root/.github/workflows/release.yaml")
mkdir -p "$tmp/publish/release/files"
touch "$tmp/publish/release/files/vmlinuz" "$tmp/publish/release/files/release.toml" "$tmp/publish/release/notes.md"
# publish <case> <release state> <expected exit> <expected call pattern> <forbidden call pattern>
publish() {
  local name=$1 got=0
  : > "$CALLS"
  (cd "$tmp/publish" && RELEASE=$2 TAG=v0.1.0 bash --noprofile --norc -e -c "$step") > "$tmp/log" 2>&1 || got=$?
  if [[ "$got" == "$3" ]] && grep -qE -- "$4" "$CALLS" && ! grep -qE -- "$5" "$CALLS"; then pass "publish: $name"; else
    fail "publish: $name (exit $got)"; sed 's/^/     /' "$CALLS" "$tmp/log"; fi
}
publish "creates the draft" none 0 \
  "^release create v0.1.0 --draft --verify-tag --title v0.1.0 --notes-file release/notes.md release/files/" "upload|edit"
publish "completes a partial draft" draft 0 \
  "^release upload v0.1.0 --clobber release/files/release.toml release/files/vmlinuz$" "create"
publish "rewrites a draft's notes" draft 0 "^release edit v0.1.0 --notes-file release/notes.md$" "create"
publish "refuses a published release" published 1 "^release view v0.1.0" "create|upload|edit"

exit "$failed"
