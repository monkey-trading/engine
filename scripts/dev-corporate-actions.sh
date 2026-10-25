#!/usr/bin/env bash
# Signed localhost TLS/API scenario with deterministic venue time and isolated temporary state.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=api::admin_tests::corporate::corporate_actions_dev_scenario_signed_dividend_split_restart_and_replay
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
