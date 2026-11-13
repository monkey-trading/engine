#!/usr/bin/env bash
# The holding-time profit fee, from protocol V55, on a dev node through the signed order and admin
# routes, the admin route over TLS, and the account and house reads, on deterministic time with
# isolated temporary state: a BTC long's closes pay the curve's fee at the hold the clock gives on
# their realised gain after the spread, an add moves the open time to the size-weighted average, a
# flip restarts it, a close-out pays at the hold of the gaining position it closes, a profit fee cut
# below launch is refused, a noticed raise applies from its time, and the public log replays to the
# same state, which a restart recovers.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::access_tests::holding_fee::dev_scenario_a_gain_pays_the_fee_its_hold_gives_and_the_fee_moves_only_up
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
