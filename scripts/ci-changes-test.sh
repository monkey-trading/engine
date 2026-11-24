#!/usr/bin/env bash
# Checks ci.yaml's `changes` classifier, run as the workflow runs it with `git diff` stubbed: the areas
# each event and path sets, from test-only modules, scenario scripts, contracts, the node's module
# families and the shared foundations to the workflow itself, a failed diff, and the events that run every job, and the range each event diffs.
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

feeds="toxicity loss_trigger rwa_feeds cme_feeds share_feeds venue_load node_load soak"
anvil="vault_deposits daily_rotation short_turns"
prebuilt="metrics_alerts alert_outbox browser_origin dashboard_data proxy_protocol batch_orders ct_watch \
  snapshot_restart signed_api production_secrets log_heads"
node="$prebuilt $anvil $feeds scenario_tests attestation_verifier recovery_scale acme engine_build"
api="signed_api browser_origin proxy_protocol batch_orders dashboard_data acme scenario_tests node_load soak engine_build"
every="rust contracts image agent_scripts merge_approval ci_changes snp_host release $node wasm_verifier"

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
case_ pull_request "" docs/trading-rules.md docs/a-new-page.md SECURITY.md
case_ pull_request "rust $feeds" crates/node/src/feeds/tests/toxicity.rs
case_ pull_request "rust" crates/core/src/volume_tier/tests.rs crates/core/tests/toxicity.rs
case_ pull_request "rust" crates/node/src/stats/tests/closeout_cost.rs crates/node/src/sequencer/resting_bench.rs
for foundation in crates/core/src/engine/mod.rs crates/wire/src/lib.rs crates/node/src/sequencer/pipeline.rs \
  crates/node/src/config/mod.rs crates/node/src/node/mod.rs crates/node/src/tee/keys.rs \
  crates/node/src/trusted_time/mod.rs crates/node/src/log/mod.rs crates/node/src/sequencer/quotes.rs Cargo.lock; do
  case_ pull_request "rust image $node wasm_verifier" "$foundation"
done
case_ pull_request "rust ct_watch engine_build" crates/node/src/ct_watch/mod.rs
for feed in crates/node/src/feeds/coinbase.rs crates/node/src/feeds/mod.rs crates/node/src/node/price_feeds.rs; do
  case_ pull_request "rust $feeds scenario_tests metrics_alerts production_secrets engine_build" "$feed"
done
for rotation in crates/node/src/handover/tail.rs crates/node/src/node/member/turn.rs crates/node/src/seed_relay/mod.rs \
  crates/node/src/sequencer/turns.rs crates/node/src/custody/registry.rs crates/node/src/api/operator.rs \
  crates/node/src/api/awards.rs; do
  case_ pull_request "rust $anvil log_heads scenario_tests recovery_scale engine_build" "$rotation"
done
for custody in crates/node/src/custody/quorum.rs crates/node/src/checkpoints/mod.rs crates/node/src/exit.rs \
  crates/node/src/withdrawals.rs crates/node/src/sequencer/payouts.rs; do
  case_ pull_request \
    "rust vault_deposits scenario_tests production_secrets log_heads snapshot_restart recovery_scale engine_build" \
    "$custody"
done
case_ pull_request "rust vault_deposits snapshot_restart $api" crates/node/src/access.rs crates/node/src/api/state.rs
for example in crates/node/examples/access_source.rs crates/core/examples/sign_request.rs; do
  case_ pull_request \
    "rust vault_deposits dashboard_data proxy_protocol batch_orders production_secrets snapshot_restart engine_build" \
    "$example"
done
case_ pull_request "rust vault_deposits $api" crates/node/src/api/stream/quotes/mod.rs crates/node/src/limits/mod.rs
case_ pull_request "rust $api" crates/node/src/api/account.rs crates/node/src/tls/mod.rs
case_ pull_request "rust attestation_verifier $api" crates/node/src/api/attestation.rs
case_ pull_request "rust recovery_scale $api" crates/node/src/api/stream/mod.rs
case_ pull_request "rust snapshot_restart recovery_scale soak engine_build" crates/node/src/snapshot/mod.rs
case_ pull_request "rust production_secrets engine_build" crates/node/src/secrets/mod.rs
case_ pull_request "rust attestation_verifier wasm_verifier signed_api dashboard_data ct_watch engine_build" \
  crates/node/src/verify/mod.rs
case_ pull_request "rust image attestation_verifier wasm_verifier signed_api ct_watch engine_build" crates/verify/src/lib.rs
case_ pull_request "rust scenario_tests dashboard_data metrics_alerts snapshot_restart node_load soak recovery_scale engine_build" \
  crates/node/src/stats/mod.rs crates/node/src/monitor/mod.rs crates/node/src/metrics/latency.rs \
  crates/node/src/storage/watch.rs
case_ pull_request "rust metrics_alerts alert_outbox ct_watch production_secrets engine_build" crates/node/src/alerts/mod.rs
case_ pull_request "rust scenario_tests dashboard_data production_secrets vault_deposits ct_watch signed_api engine_build" \
  crates/node/src/admin/mod.rs
case_ pull_request "rust $anvil $feeds scenario_tests attestation_verifier recovery_scale" crates/node/src/test_support/mod.rs
case_ pull_request "rust $anvil" crates/node/src/custody/tests/election_scenario.rs crates/node/src/log_check/tests.rs
case_ pull_request "rust scenario_tests" crates/node/src/api/access_tests/spreads.rs crates/core/tests/hot_path_benchmark.rs
case_ pull_request "rust scenario_tests" crates/core/tests/launch_book_benchmark.rs
case_ pull_request "rust scenario_tests" crates/core/tests/launch_book_benchmark/cases.rs
case_ pull_request "rust scenario_tests" crates/node/src/stats/launch_bench.rs
case_ pull_request "rust scenario_tests" crates/core/tests/mass_events_benchmark.rs crates/core/tests/mass_events_benchmark/closeouts.rs
case_ pull_request "rust scenario_tests" crates/node/src/sequencer/wait_bench/mass.rs
case_ pull_request "rust scenario_tests" crates/node/src/stats/bench.rs
case_ pull_request "rust scenario_tests node_load soak recovery_scale" config/production-genesis.toml
case_ pull_request "rust scenario_tests node_load soak recovery_scale" research/cap-calibration/data/derived/market_mix.csv
case_ pull_request "rust scenario_tests node_load soak recovery_scale" crates/core/tests/common/launch_book/build.rs
case_ pull_request "rust node_load soak recovery_scale" crates/node/src/load_scenario/mod.rs \
  crates/node/src/load_scenario/generator/traders.rs
case_ pull_request "rust scenario_tests node_load soak recovery_scale" crates/node/src/sequencer/seeded.rs
case_ pull_request "rust recovery_scale soak" crates/node/src/sequencer/recovery_scale/mod.rs \
  crates/node/src/sequencer/recovery_scale/flow.rs
case_ pull_request "rust node_load soak vault_deposits" ops/runbooks/production-node.toml
case_ pull_request "rust vault_deposits scenario_tests node_load soak recovery_scale" ops/runbooks/production-deployment.toml
case_ pull_request "rust" config/ct-watch.toml
case_ pull_request "rust attestation_verifier" crates/node/src/verify/tests/mod.rs
case_ pull_request "rust short_turns" crates/core/tests/short_turns.rs
case_ pull_request "rust wasm_verifier" crates/verify-wasm/src/lib.rs
case_ pull_request "contracts $anvil production_secrets log_heads engine_build" contracts/src/Vault.sol
case_ pull_request "contracts" contracts/test/Vault.t.sol
case_ pull_request "rust contracts" contracts/test/fixtures/withdrawal.json
case_ pull_request "rust image $node" config/dev.toml
case_ pull_request "toxicity" scripts/dev-toxicity.sh
case_ pull_request "signed_api engine_build" scripts/dev-status-notice.sh
case_ pull_request "venue_load" scripts/dev-venue-load.sh
case_ pull_request "node_load" scripts/dev-load.sh
case_ pull_request "soak" scripts/dev-soak.sh
case_ pull_request "recovery_scale" scripts/dev-recovery-scale.sh
case_ pull_request "share_feeds" scripts/dev-share-feeds.sh
case_ pull_request "scenario_tests" scripts/dev-holding-fee.sh
case_ pull_request "ct_watch contracts $anvil production_secrets log_heads engine_build" \
  scripts/dev-ct-log.py contracts/test/mocks/MockUSDC.sol
case_ pull_request "$node wasm_verifier" scripts/dev-unlisted.sh
case_ pull_request "image acme release" image/retry
case_ pull_request "ci_changes" scripts/ci-changes-test.sh
for snp in scripts/snp-host-check.sh scripts/snp-host-check-test.sh scripts/snp-launch.sh scripts/snp-launch-test.sh; do
  case_ pull_request "snp_host" "$snp"
done
case_ pull_request "rust" scripts/snp-node-config.py
case_ pull_request "release" .github/workflows/release.yaml .github/workflows/release-build.yaml
case_ pull_request "release" scripts/release-gate.sh scripts/release-test.sh
case_ pull_request "rust release" scripts/release-files.py
case_ pull_request "rust" config/release/production.toml config/release/staging.toml
case_ pull_request "$every" .github/actions/build-image/action.yaml
case_ pull_request "" scripts/snp-local-chain.sh scripts/snp-bench-guest.sh
case_ merge_group "rust contracts snp_host" scripts/snp-launch.sh
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
