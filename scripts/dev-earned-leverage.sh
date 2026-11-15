#!/usr/bin/env bash
# Earned leverage, from protocol V64, on a dev node through the signed order and withdrawal routes and
# the account read, on deterministic time with isolated temporary state: a fresh wallet holding $10,000
# is refused a BTC-USD short at 25x, which tier 0's 5% margin floor does not admit, and opens 19.8x; at
# tier 1 it opens 50x and at tier 2 about 100x, BTC's own 1%; thirty days on its tier falls, the short
# stays open, a withdrawal stops at the new initial requirement, and BTC rising closes it out only at
# the market's bands; the public log replays to the same state, and a restart recovers it. The node's
# tier table is dev's with the production genesis's margin floors and thresholds a few fills reach in
# a day.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::access_tests::earned_leverage::dev_scenario_a_tiers_margin_floor_caps_leverage_and_withdrawals
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
