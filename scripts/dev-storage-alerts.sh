#!/usr/bin/env bash
# The disk alerts on real files and a real volume, in a temporary directory the test makes and removes: a node's six
# stores are written, the watcher runs against them with a free-share threshold above the volume's free space, and
# `disk_space_low` is raised naming the volume, the stores' bytes and the volume's space are exported as metrics, a
# store whose directory stops listing raises `storage_unreadable`, and the alert resolves once it lists again. The
# thresholds' arithmetic, the projection and the metrics are covered by the module's unit tests.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=storage::tests::scenario::dev_scenario_a_volume_past_its_threshold_alerts_and_an_unreadable_store_pages_and_resolves
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
