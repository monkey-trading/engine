#!/usr/bin/env bash
# The seven-day notice on the stress parameters and a listed market's markup, from protocol V49, on a
# dev node through `engine admin`, the signed admin route and TLS on deterministic time with isolated
# temporary state: each change is refused with no notice and before its time, applies at it, and the
# public log replays to the same state.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::admin_tests::notice::dev_scenario_stress_parameters_and_a_markup_move_only_after_a_seven_day_notice
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
