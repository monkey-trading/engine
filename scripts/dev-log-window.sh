#!/usr/bin/env bash
# The log's local window on the host, on accelerated sequenced time with isolated temporary state and a loopback
# stand-in for the archive's bucket: a dev node with a window of six hours, which only a development build takes,
# writes days of flow and a daily snapshot at each UTC day's first checkpoint, and keeps on its host the window and
# no more than a day past it, however long it runs; with the bucket down, nothing leaves the host and
# `archive_upload_lagging` fires, and once it is back the window moves on; a segment the host loses before the bucket
# holds it pages Critical `log_segment_missing` until a member's copy uploads its ranges, from which the node puts it
# back; the node, and a member backfilling an empty copy from the node's archive, each start from the newest daily
# snapshot without the segments before it; and `engine replay --log <the node's URL>` replays the whole log from
# genesis to the root of the node's last checkpoint. The window's unit tests run beside it.
# Passes with `PASS: the host keeps the window and the newest snapshot, ...`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=archive::window::scenario::dev_scenario_log_window
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact --include-ignored --nocapture 2>&1) || {
  echo "$output"
  exit 1
}
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
output=$(cargo test -p engine-node --lib --locked archive::window 2>&1) || { echo "$output"; exit 1; }
grep -E '^test result' <<<"$output"
