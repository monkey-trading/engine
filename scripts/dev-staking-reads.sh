#!/usr/bin/env bash
# Runs the staking reads scenario against an isolated Anvil: forty candidates register under a cap of
# 100 and the timelock approves thirty-five, whom stakers delegate to; the node lists them at
# GET /v1/operators through the registry lens, read at a final block through the custody quorum, with
# the votes the chain shows; three distribution epochs close and GET /v1/monkey/distributions lists
# them; stakers delegate again and a later list shows the new votes; a restart keeps the epoch list.
# The scenario verifies every signed answer and saves it, and `engine verify-response` then verifies
# each saved answer on its own.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
answers="$(mktemp -d)"
trap 'rm -rf "$answers"' EXIT
(cd "$root/contracts" && forge build)
STAKING_READS_ANSWERS="$answers" CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" \
  -p engine-node --lib \
  custody::tests::election_scenario::staking_reads::dev_scenario_the_operator_list_and_distribution_history_are_signed_reads \
  -- --ignored --exact --nocapture
CARGO_INCREMENTAL=0 cargo build --locked --manifest-path "$root/Cargo.toml" -p engine-node
engine="${CARGO_TARGET_DIR:-$root/target}/debug/engine"
read -r signer chain_id vault < "$answers/deployment"
count=0
while IFS=$'\t' read -r n challenge path; do
  "$engine" verify-response --signer "$signer" --chain-id "$chain_id" --vault "$vault" \
    --challenge "$challenge" --path "$path" --headers "$answers/$n.headers" --body "$answers/$n.body"
  count=$((count + 1))
done < "$answers/answers"
[ "$count" -ge 8 ] || { echo "only $count answers were saved" >&2; exit 1; }
echo "engine verify-response verified $count answers"
