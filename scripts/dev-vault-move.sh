#!/usr/bin/env bash
# Runs the move to a new vault dev scenario on an isolated Anvil chain, with time warped by the chain and the
# node's test clock together, on the parameter votes scenario's stack: A, B and C register, stake and delegate
# 40, 30 and 30 thousand MONKEY, a close adopts them as the rotation set, and the alpha's node posts checkpoints
# to its vault. It checks:
# that a trader's losses earn MONKEY, part of which it claims from the minter against the alpha's root, and that
# a High award the set passes is paid in part by a close;
# that once the timelock turns rotation off, `engine carry` builds the carry from the alpha's last accepted
# checkpoint and refuses one from while rotation was on;
# that after the guardian retires the alpha's signer and one timelock batch moves the registry's custody and
# MONKEY's minter to a launch vault, the launch node passes its start's checks and its genesis carries every
# entitlement and award, each award unpaid, with no period before its first command allocating, and no stake: the
# launch node mirrors the staking contract's events again from its deployment, to each stake the alpha held;
# that the trader claims the rest of its MONKEY against the launch vault, the minter's total equal to the carried
# entitlement, a new stake mirrors once past the carried frontier, and a close pays the carried award from its
# whole amount;
# and that the launch log replays and checks against the alpha's, while a carry file with an entitlement raised
# by one unit is refused. It prints the carry's size and the mirror's catch-up time.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib -- \
  --ignored --exact --nocapture \
  custody::tests::election_scenario::vault_move::dev_scenario_the_launch_log_carries_monkey_and_awards_and_mirrors_the_stakes_again
