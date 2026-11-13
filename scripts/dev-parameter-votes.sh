#!/usr/bin/env bash
# Runs the parameter votes dev scenario on an isolated Anvil chain, with time warped by the chain and the
# node's test clock together. A, B and C register and delegate 40, 30 and 30 thousand MONKEY, and a
# close adopts them as the rotation set; once it is in effect its members vote on the ParameterVote
# contract and the custody watcher mirrors each passed proposal into the real sequencer. It checks:
# that a BTC margin raise passing with 70% of the weight applies seven warped days after it is logged,
# while an ETH raise holding 60% never passes and is listed open by /v1/operators; that a fast-path
# stress floor at level 2 applies at once, survives the operator's key setting its own floor to 0, and
# lapses at its one-day lifetime; that a fast-path margin cut passes on chain and the engine refuses
# it; that the operator's signed profit-fee SetParams is refused while rotation is on; that
# /v1/house/state lists the waiting change and the restriction with their times; and that the log
# replays to its roots.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib -- \
  --ignored --exact --nocapture \
  custody::tests::election_scenario::parameter_votes::dev_scenario_members_vote_parameter_changes_by_stake
