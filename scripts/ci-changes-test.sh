#!/usr/bin/env bash
# Checks ci.yaml's `changes` classifier, run as the workflow runs it with `git diff` stubbed: the areas
# each event and path sets, from test-only modules, scenario scripts, contracts and the node's leaves to
# the workflow itself, a failed diff, and the events that run every job, and the range each event diffs.
# Prints one line per case and exits 1 if any case fails. Needs yq.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
script="$(yq '.jobs.changes.steps[] | select(.id == "classify") | .run' "$root/.github/workflows/ci.yaml")"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"
# Records the range it was asked for and lists $FILES as the diff, or fails as a missing base would.
cat > "$tmp/bin/git" <<'EOF'
#!/usr/bin/env bash
echo "${*: -1}" > "$RANGE_FILE"
[[ "$1" == diff && -z "${DIFF_FAILS:-}" ]] || exit 128
printf '%s\n' "$FILES"
EOF
chmod +x "$tmp/bin/git"

feeds="toxicity loss_trigger rwa_feeds cme_feeds"
anvil="vault_deposits daily_rotation short_turns"
prebuilt="metrics_alerts alert_outbox browser_origin dashboard_data proxy_protocol batch_orders ct_watch \
  snapshot_restart signed_api production_secrets log_heads"
node="$prebuilt $anvil $feeds scenario_tests attestation_verifier acme engine_build"
every="rust contracts image agent_scripts merge_approval ci_changes $node wasm_verifier"

failed=0
export RANGE_FILE="$tmp/range"
# case <event> <expected areas> <file>...: the runner's shell, with a pull request's base and head
# commits `base` and `head`, and the group's or push's commit `group`.
case_() {
  local event=$1 expected=$2; shift 2
  local out="$tmp/output" got want
  : > "$out"; : > "$RANGE_FILE"
  FILES="$(printf '%s\n' "$@")" PATH="$tmp/bin:$PATH" EVENT=$event BASE=base HEAD=head SHA=group \
    GITHUB_OUTPUT=$out bash --noprofile --norc -eo pipefail -c "$script" > /dev/null
  got=$(sed -n 's/=true$//p' "$out" | sort | xargs)
  want=$(tr -s ' ' '\n' <<<"$expected" | sort -u | xargs)
  if [[ "$got" == "$want" ]]; then
    echo "ok   $event $*"
  else
    echo "FAIL $event $*"; echo "     want: $want"; echo "     got:  $got"; failed=1
  fi
}

case_ pull_request "" README.md agents/AGENTS.md research/launch-caps/run.py
case_ pull_request "rust $feeds" crates/node/src/feeds/tests/toxicity.rs
case_ pull_request "rust" crates/core/src/volume_tier/tests.rs crates/core/tests/toxicity.rs
case_ pull_request "rust" crates/node/src/stats/tests/closeout_cost.rs crates/node/src/sequencer/resting_bench.rs
case_ pull_request "rust $prebuilt $anvil $feeds scenario_tests attestation_verifier acme engine_build image wasm_verifier" \
  crates/core/src/engine/mod.rs
case_ pull_request "rust image ct_watch engine_build" crates/node/src/ct_watch/mod.rs
case_ pull_request "rust image vault_deposits" crates/node/src/exit.rs
case_ pull_request "rust image $feeds production_secrets engine_build" crates/node/src/feeds/coinbase.rs
case_ pull_request "rust $anvil $feeds scenario_tests attestation_verifier" crates/node/src/test_support/mod.rs
case_ pull_request "rust $anvil" crates/node/src/custody/tests/election_scenario.rs crates/node/src/log_check/tests.rs
case_ pull_request "rust scenario_tests" crates/node/src/api/access_tests/spreads.rs crates/core/tests/hot_path_benchmark.rs
case_ pull_request "rust attestation_verifier" crates/node/src/verify/tests/mod.rs
case_ pull_request "rust short_turns" crates/core/tests/short_turns.rs
case_ pull_request "rust wasm_verifier" crates/verify-wasm/src/lib.rs
case_ pull_request "contracts $anvil production_secrets log_heads engine_build" contracts/src/Vault.sol
case_ pull_request "contracts" contracts/test/Vault.t.sol
case_ pull_request "rust contracts" contracts/test/fixtures/withdrawal.json
case_ pull_request "rust image $node" config/dev.toml
case_ pull_request "toxicity" scripts/dev-toxicity.sh
case_ pull_request "scenario_tests" scripts/dev-holding-fee.sh
case_ pull_request "ct_watch contracts $anvil production_secrets log_heads engine_build" \
  scripts/dev-ct-log.py contracts/test/mocks/MockUSDC.sol
case_ pull_request "$node wasm_verifier" scripts/dev-unlisted.sh
case_ pull_request "image acme" image/retry
case_ pull_request "ci_changes" scripts/ci-changes-test.sh
case_ pull_request "$every" .github/workflows/ci.yaml
case_ pull_request "$every" a-path-no-arm-names
DIFF_FAILS=1 case_ pull_request "$every" README.md
case_ merge_group "rust contracts" README.md
case_ merge_group "rust contracts $feeds" crates/node/src/feeds/tests/toxicity.rs
case_ push "$every" README.md
case_ schedule "$every" README.md
case_ workflow_dispatch "$every" README.md
# range <event> <expected range, or nothing for no diff>
range() {
  case_ "$1" "$every" a-path-no-arm-names > /dev/null
  if [[ "$(cat "$RANGE_FILE")" == "$2" ]]; then
    echo "ok   $1 diffs '$2'"
  else
    echo "FAIL $1 diffs '$(cat "$RANGE_FILE")', not '$2'"; failed=1
  fi
}
range pull_request base...head
range merge_group origin/main...group
range push ""
exit "$failed"
