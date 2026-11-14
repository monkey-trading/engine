#!/usr/bin/env bash
# Volume tiers, from protocol V59, on a dev node through the signed order route and the account and
# house reads, on deterministic time with isolated temporary state: a fresh wallet trades at the best
# price only up to a quarter of BTC-USD's first band, churn closed within 60 s and round trips count
# nothing, an open that crosses midnight counts on the day it matures, pending volume grants nothing,
# and $500k counted on each of two days reaches tier 1, whose first band ends at $1M; the public log
# replays to the same state, which a restart recovers.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::access_tests::volume_tiers::dev_scenario_counted_volume_earns_a_tier_and_more_size_at_the_best_price
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
