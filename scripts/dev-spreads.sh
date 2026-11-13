#!/usr/bin/env bash
# The V52 spreads on a dev node, through the signed order and admin routes, the admin route over TLS,
# and the signed quote stream, on deterministic time with isolated temporary state: two accounts open
# and close opposite BTC positions of $1m, $5m and $12m in legs through every band and one trades EUR-USD
# at its 0.4bp first band, each fill paying its band's charge on the open and the close; the quote
# stream steps through the bands a close crosses and covers every smaller order; listings below a
# floor, a band change without notice and a 0x multiplier are refused; a noticed band and margin change
# applies from its time; a ceiling below the launch close charge is refused; a market listed through
# a bound adoption is refused another listing and while its charge exceeds its ceiling, then lists;
# the public log replays to the same state, and a restart recovers it.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::access_tests::spreads::dev_scenario_every_fill_pays_its_spread_and_curves_move_only_after_notice
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
