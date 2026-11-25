#!/usr/bin/env bash
# The account history index bounded by its retention, on deterministic time with isolated temporary state: a node
# with [snapshots] and a ten-minute history.retention takes deposits for six retentions, two recovery caches
# pinning its recent versions as a node's do; once its first events expire, the index's file stops growing while
# the log grows, and every page states where its history begins and holds every logged event from then on.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=node::paired_tests::retention::dev_scenario_history_retention
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact --nocapture 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
