#!/usr/bin/env bash
# The house loss trigger's dev scenario (protocol V59): loopback Coinbase and Kraken stand-ins price
# BTC-USD through a sequencer at the latest protocol, under the dev genesis with production's loss
# threshold of zero at level 2 and a 30 s cooldown. Twice, a client buys at level 0 and the stand-ins
# move its way until the house has lost money over the day: BTC rises to level 2 on that move, the
# house's loss then is at most that one move's, and the client's next buy pays level 2's spread; the
# stand-ins move back until the house is ahead, BTC holds level 2 through the cooldown and steps down,
# and the client's close pays the band alone. The log then replays to every root, and a restart
# recovers the same loss. Needs only Cargo: the venues are local stand-ins. About a minute and a half.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
test=feeds::tests::loss_trigger::dev_scenario_loss_trigger
output=$(CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib "$test" \
  -- --ignored --exact --nocapture 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
