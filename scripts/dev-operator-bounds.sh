#!/usr/bin/env bash
# The operator's bounds on a dev node, through the real publisher worker, signed admin API and TLS on
# deterministic time with isolated temporary state: a benchmark rate inside the cap applies and one
# outside it is refused, the worker leaves a refused rate out and signs the rest, a stress floor at
# level 2 is refused and one at level 1 lapses after 72 hours, and a restart and a public replay
# reach the same state.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::benchmark_tests::operator_bounds::dev_scenario_the_operator_is_bounded_on_benchmark_rates_and_the_stress_floor
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
