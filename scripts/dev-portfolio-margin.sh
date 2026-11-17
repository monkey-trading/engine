#!/usr/bin/env bash
# Portfolio margin as a read on the account, on a dev node under config/dev.toml's fixture group,
# BTC-USD and ETH-USD, through the signed order routes and the account read on deterministic time,
# with isolated temporary state: account A opens $1M long BTC-USD and $1M short ETH-USD, account B
# $1M long BTC-USD alone, and each read is verified as `engine verify-response` verifies it. A's
# matched exposure takes the group's credit, leaving its portfolio requirement below its per-position
# requirement and above the group's floor share of it, naming the group and its binding direction;
# B's matches nothing and equals its per-position requirement; after BTC-USD
# moves 2% both reads follow the new marks; and once A closes its ETH-USD leg its portfolio
# requirement equals its per-position requirement.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::access_tests::portfolio_margin::dev_scenario_a_hedged_pair_reads_a_portfolio_requirement_below_its_bands
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
