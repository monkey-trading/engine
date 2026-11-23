#!/usr/bin/env bash
# Runs the bug bounty awards dev scenario on an isolated Anvil chain, with time warped by the chain and the
# node's test clock together, on the parameter votes scenario's stack: A, B and C register, stake and
# delegate 40, 30 and 30 thousand MONKEY, a close adopts them as the rotation set, and the node posts
# checkpoints to the vault. It checks:
# that a High award A proposes and a Low award B proposes pass on the ParameterVote contract and apply
# seven warped days after they are logged, the High award first in the queue;
# that the operator's own award is refused, and a fast-path award and an award at another amount than its
# severity's pass on chain and are refused by the engine;
# that closes above the liquidity floor pay the front award 5% of each excess first, before the operator's
# share and the stakers, while the Low award waits behind it, and that a close below the floor pays
# nothing;
# that the next close finishes the High award and pays the Low one, and each researcher claims the award
# from the vault with a distribution proof against a posted root;
# that /v1/awards lists the queue and what each award has been paid; and that the log replays to its roots.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib -- \
  --ignored --exact --nocapture \
  custody::tests::election_scenario::bounty_awards::dev_scenario_voted_awards_are_paid_from_closes_first_and_claimed_from_the_vault
