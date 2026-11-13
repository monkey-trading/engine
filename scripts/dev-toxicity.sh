#!/usr/bin/env bash
# The toxicity score's dev scenario (protocol V54): loopback Coinbase and Kraken stand-ins price
# BTC-USD through a sequencer at the latest protocol, under the dev genesis, whose BTC-USD markout
# floor is a labelled 1 bp fixture. A toxic client's 25 buys each see the stand-ins move 1 bp its way
# a moment later: it pays no add-on until 20 fills have scored, and then the add-on of the tier the
# test computes from the moves. An edgeless client's 25 fills on shuffled sides, while the books
# wander on their own, stay at the best price. The toxic client's round trips at the index between
# moves leave its score at its tier. The log then replays to every root, and a restart recovers the
# same score. Needs only Cargo: the venues are local stand-ins. About four minutes, or eight when it
# starts within four minutes of an hour, the score's decay step.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  feeds::tests::toxicity::dev_scenario_toxicity -- --ignored --exact --nocapture
