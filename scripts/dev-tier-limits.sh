#!/usr/bin/env bash
# Request and order limits, from protocol V66, on a dev node through the signed order route and the
# account read, on deterministic time with isolated temporary state: a fresh wallet rests 25 orders
# and the 26th is refused naming tier 0; its next request and ten signed orders after it get 429
# naming tier 0's 2 requests a second; a wallet that has reached tier 2 sends the same requests and
# has them accepted, and rests 30 orders; thirty days on its tier falls, its 30 orders stay and a new
# one is refused at tier 0's 25; the public log replays to the same state, and a restart recovers it.
# The node's tier table is dev's with the production genesis's limits and thresholds a few fills
# reach in a day.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::access_tests::tier_limits::dev_scenario_a_tiers_limits_cap_resting_orders_and_requests
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
