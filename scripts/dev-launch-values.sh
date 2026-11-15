#!/usr/bin/env bash
# Runs the launch values scenario against an isolated Anvil: the vault, its MONKEY token, the
# timelock and the staking contract deploy from ops/runbooks/production-deployment.toml at the
# decided values, and the node's InitEmissions and InitStaking checks read them back (the budget from
# the vault, the epoch and cooldown through its custody quorum), accept those values and refuse any
# other budget, epoch or cooldown.
# Needs Anvil, Forge, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  api::admin_tests::launch_scenario::dev_scenario_the_launch_values_deploy_and_the_nodes_initialisations_accept_them \
  -- --ignored --exact
