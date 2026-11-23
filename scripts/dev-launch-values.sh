#!/usr/bin/env bash
# Runs the launch values scenario against an isolated Anvil: the vault, the MONKEY minter and its
# token, the timelock and the staking contract deploy from ops/runbooks/production-deployment.toml at
# the decided values, and the node's InitEmissions and InitStaking checks read them back (the budget
# from the minter, the epoch and cooldown through its custody quorum), accept those values and refuse any
# other budget, epoch or cooldown. Then, on a second Anvil, the rotation contracts deploy beside the
# vault and the registry from the same file: the log heads, the seed receiver, the operator schedule
# at the launch heartbeat rule, the parameter vote and the fault proof. The timelock names the schedule
# in the registry. The node reads the heartbeat rule through its custody quorum and does not refuse it,
# and its custody watcher, which checks the registry's schedule, the schedule's turn length, the vote
# contract's schedule and the fault proof's vault, mirrors past the deployment.
# Needs Anvil, Forge, Cargo, Python 3, and installed Soldeer dependencies. No external RPC or keys are
# used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  api::admin_tests::launch_scenario::dev_scenario_the_launch_values_deploy_and_the_nodes_initialisations_accept_them \
  -- --ignored --exact
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  custody::tests::election_scenario::launch_schedule::dev_scenario_the_rotation_contracts_deploy_at_the_launch_values_and_the_node_accepts_them \
  -- --ignored --exact --nocapture
