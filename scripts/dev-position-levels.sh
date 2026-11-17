#!/usr/bin/env bash
# Take-profit and stop-loss levels, from protocol V79, on a dev node through the signed order, levels,
# cancel-all and withdrawal routes, the account and market reads, the history and the operator's stress
# floor over TLS, on deterministic time with isolated temporary state and BTC-USD's index starting at
# 100,000: a long opened naming both levels, and a stop loss inside BTC's 30 bp minimum distance of its
# closing price or above it refused as level_too_close; the levels kept through an add, a partial close
# and Cancel all; one price far below the stop loss filling the whole position at its closing price; a
# stop loss just inside the band firing on a raised stress floor with the index unchanged; a take profit
# filling at its level or better once the closing price reaches it; a resting buy limit's levels
# attaching to its long; a stop loss above the close-out level filling with no close-out; levels removed
# by a reversal and by a close-out, each naming why; a tier-0 wallet's levels taking its 25th order
# slot and refused at the 26th naming tier 0; a WTI stop loss filling at the first price after the
# weekend reopening; and the history by kind, the public log's replay and a restart reporting the same
# levels. The node's tier table is dev's with the production genesis's resting-order limits.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::access_tests::levels_scenario::dev_scenario_levels_close_a_whole_position_at_its_closing_price_and_follow_it
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
