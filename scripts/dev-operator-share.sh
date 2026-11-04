#!/usr/bin/env bash
# Runs the operator's share scenario against an isolated Anvil: the operator registry elects a
# candidate that the custody watcher mirrors into the sequencer, a client's losses put the house
# above the liquidity floor, and an epoch's close credits the candidate a fifth and stakers the rest.
# The candidate and a staker each claim through the vault against the posted root, an epoch below the
# floor pays neither, and the candidate's unclaimed leaf registers and pays through the vault's exit,
# from the stopped node's public log. Then, on a fresh Anvil, a second election takes effect halfway
# through a distribution epoch: the close credits the outgoing and incoming candidates about half the
# operator's share each, by their time in office, both parts show in their accounts' histories, and
# the vault pays each claim.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
# One at a time: each starts its own Anvil and node.
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib -- \
  --ignored --exact --test-threads=1 \
  custody::tests::election_scenario::operator_share::dev_scenario_the_elected_operator_and_a_staker_each_claim_their_share \
  custody::tests::election_scenario::time_in_office::dev_scenario_a_handover_halfway_through_an_epoch_splits_the_operators_share
