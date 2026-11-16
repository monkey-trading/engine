#!/usr/bin/env bash
# Runs the short-turns scenario (#850) on a development build with five-minute operator turns (the
# `short-turns` feature, which a production build refuses), on real nodes and true time. On an
# isolated Anvil chain whose blocks stand in for Arbitrum's, A, B and C register with standby keys the
# vault's signer vouches for, a close adopts them, and the turn in effect now is drawn for A. The chain
# then runs on true time, and A, B and C run the node binary's own start as child processes, A holding
# the turn and B and C following it. Once A's log has named the set a turn ahead, the next two turns are
# drawn for B and then C. At each of the two boundaries the turn's member takes the keys by itself and
# logs TurnTaken, and its admin key signs while the previous turn's is refused; with C, the incumbent,
# killed, the members' liveness callers promote the next member, whose node serves from its copy under
# its standby key; and its log replays across the change of signer. It takes about twenty minutes.
# Needs Anvil, Forge, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --features short-turns \
  --lib -- --ignored --exact --nocapture \
  custody::tests::election_scenario::liveness::promoted::short_turns::dev_scenario_five_minute_turns_hand_over_and_promote
