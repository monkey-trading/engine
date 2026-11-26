#!/usr/bin/env bash
# A node's start from its newest daily snapshot and the log after it, on deterministic time with isolated temporary
# state: a node writing launch flow on a small book across two UTC midnights publishes three snapshots; with the log's
# segments before the newest snapshot's checkpoint moved out, it starts from that snapshot reading only the log after
# it, to what a whole replay gives, and sequences; a member with an empty copy starts its replica from the snapshot and
# takes the node's turn through the tail handover; a snapshot with altered carried state is passed over with Warning
# `snapshot_start_refused`; and the whole log replays in ranges from the snapshots, each one's carried state checked.
# Then a node whose log before a snapshot is gone serves the chart history and the dashboard series it served before
# the start, a marks row before the snapshot deleted from its cache is refused at serve time, and an altered frozen
# copy of the cache is refused at the start; a start under keys sealed afresh on the same host, as `--new-key` seals
# them, serves the chart history from before the snapshot on the root over the copy's rows, with no whole replay, while
# a snapshot re-signed without that root starts the chart history at the snapshot with Warning `marks_cache_rebuilt`;
# a signed snapshot whose replay state does not check gives way to a whole replay; and the node, started from its
# snapshot, publishes the next day's whose replay state the whole log folds.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
export CARGO_INCREMENTAL=0
tests=(
  sequencer::recovery_scale::snapshot_start::dev_scenario_snapshot_start
  node::recovery::history_tests::a_start_from_a_daily_snapshot_answers_as_the_node_did_before_it
  node::recovery::history_tests::an_altered_frozen_copy_starts_the_chart_history_at_the_snapshot
  node::recovery::history_tests::a_start_under_new_keys_keeps_the_chart_history_before_the_snapshot
  node::recovery::history_tests::a_snapshot_the_start_cannot_go_on_from_gives_way_to_a_whole_replay
  node::recovery::history_tests::a_snapshot_written_after_a_start_from_a_snapshot_carries_what_the_whole_log_folds
)
for test in "${tests[@]}"; do
  output=$(cargo test -p engine-node --lib --locked "$test" -- --exact --include-ignored --nocapture 2>&1) || { echo "$output"; exit 1; }
  echo "$output"
  # A renamed or moved test would otherwise pass by running nothing.
  grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test $test did not run" >&2; exit 1; }
done
