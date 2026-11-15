#!/usr/bin/env bash
# Tier benefits, from protocol V68, on a dev node through the signed order and admin routes and the
# account read, on deterministic time with isolated temporary state: a wallet at tier 2 and one at
# tier 0 each hold a winning BTC-USD position 45 minutes and pay 9% and 10% of the gain; each holds
# one over a rollover and the tier 2 wallet's markup is a tenth lower; each closes one after 2 minutes
# and both pay the hold curve's rate; each loses the same in one MONKEY period and the tier 2 wallet's
# weight is 1.25 times its loss; the public log replays to the same state, and a restart recovers it.
# The node's tier table is dev's with the activation's benefits and thresholds a few fills reach.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::access_tests::volume_benefits::dev_scenario_a_higher_tier_pays_less_fee_and_financing_and_earns_more_emissions
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
