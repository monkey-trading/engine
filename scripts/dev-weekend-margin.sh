#!/usr/bin/env bash
# The weekend margin, from protocol V59, on a dev node through the signed order and withdrawal routes,
# the market and account reads, and the admin route over TLS, on deterministic time with isolated
# temporary state: a 100x WTI long opened mid-week cannot add from 2 hours before Friday's close, is
# closed out at the last price before it when the weekend gap starts while a long that holds the 7.5%
# weekend margin stays and may withdraw only what leaves it over the gap, the normal margin applies
# again at Sunday's open, a calendar change that would narrow the gap and a relisting that moves the
# weekend margin without notice are refused, and the public log replays to the same state, which a
# restart recovers.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::access_tests::weekend_margin::dev_scenario_wti_meets_its_weekend_margin_from_two_hours_before_the_close
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
