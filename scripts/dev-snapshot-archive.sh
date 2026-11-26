#!/usr/bin/env bash
# The daily snapshots' archive, on deterministic time with isolated temporary state and a loopback stand-in for the
# bucket behind TLS from a test CA, which checks each upload's signature:
#   1. a node writing launch flow on a small book across four UTC midnights publishes a daily snapshot each simulated
#      day, and its uploader puts each into the bucket as it is published and reads it back;
#   2. the snapshots stamped more than the window before the newest leave the host, but for the newest of them, the
#      manifest still lists them, and `GET /v1/archive/snapshots/<file>` answers 308 to the bucket's object;
#   3. `engine replay --snapshots <the node's URL>` downloads every listed snapshot, those gone from the host from the
#      bucket, and replays the log in ranges to what the whole log replays to;
#   4. a member's snapshot start takes a snapshot the node no longer holds from the bucket;
#   5. a bucket object whose bytes are altered is refused by the replay and by the member's start.
# Then the uploader's, the archive's and the readers' tests: never removed before it is confirmed, a mismatch or a
# damaged file kept on the host, the lag alert, and a redirect followed only to the reader's own bucket.
# Passes with `PASS: every daily snapshot is in the bucket, and the host keeps the window's`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export CARGO_INCREMENTAL=0
tests=(
  sequencer::recovery_scale::snapshot_archive::dev_scenario_snapshot_archive
  daily_snapshots::upload::tests::a_snapshot_leaves_the_host_once_confirmed_and_older_than_the_window_but_the_newest_before_it
  daily_snapshots::upload::tests::an_object_that_reads_back_otherwise_keeps_the_hosts_copy
  daily_snapshots::upload::tests::a_file_that_differs_from_its_listing_creates_no_object
  daily_snapshots::upload::tests::a_snapshot_waiting_past_the_lag_alert_is_paged_apart_from_the_logs_ranges
  daily_snapshots::fetch::tests::a_snapshot_the_node_redirects_is_read_from_the_bucket
  api::archive_tests::a_listed_snapshot_the_host_no_longer_holds_redirects_to_its_object
)
for test in "${tests[@]}"; do
  output=$(cargo test -p engine-node --lib --locked "$test" -- --exact --include-ignored --nocapture 2>&1) || { echo "$output"; exit 1; }
  echo "$output"
  # A renamed or moved test would otherwise pass by running nothing.
  grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test $test did not run" >&2; exit 1; }
done
