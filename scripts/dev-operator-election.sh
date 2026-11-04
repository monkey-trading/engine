#!/usr/bin/env bash
# Runs the operator election scenario against an isolated Anvil: four accounts stake, two register
# as candidates with self-bonds, and the timelock approves their releases; one staker delegates
# amounts to both candidates, moves and takes amounts back, and unstakes past its undelegated stake,
# which reduces each delegation in proportion; the others delegate; a close while rotation is off
# elects nothing; the timelock turns rotation on, and a later close elects the leader by those
# tallies, which the custody watcher mirrors through three local RPC proxies into the
# sequencer. Past the effective time the deployer's admin commands are refused and the elected key's
# apply, the elected self-bond cannot be unstaked, and the log replays.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  custody::tests::election_scenario::dev_scenario_stakers_elect_the_key_that_signs_from_the_effective_time \
  -- --ignored --exact
