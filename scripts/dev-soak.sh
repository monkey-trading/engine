#!/usr/bin/env bash
# The soak dev scenario (#909): the node load scenario (`dev-load.sh`) run for hours at ten times
# launch flow, on a node that runs the launch configuration's storage, with #867's faults and restarts in it.
#
#   ./scripts/dev-soak.sh [hours]               8 hours unless given
#   ./scripts/dev-soak.sh --rotation [hours]    the rotation leg instead, 2 hours unless given
#
# By hand before each release, in a quiet window, under `nice`, never at 100,000 accounts on a
# machine that is also doing other work, and with the report kept for the release notes
# (`ops/runbooks/deploy-and-rollback.md`):
#   SOAK_DIR=<a directory on a disk with room> nice ./scripts/dev-soak.sh 2>&1 | tee soak-report.txt
# SOAK_LOG_DIR=<a directory on another volume> puts the log in its `log` directory there, apart from SOAK_DIR's
# caches and history index, as a node whose log has a disk of its own has it; the data is kept with the run.
# Name SOAK_DIR: the log of a full run is large (see the report's `disk_bytes` lines), and without it
# the run writes under the temporary directory and removes it afterwards. With it the data is kept;
# delete it by its exact path once the report is saved.
#
# The run is the load scenario's: the launch book of LOAD_ACCOUNTS (100000) accounts on the production
# genesis's 34 markets taken over by a node in one process, its feeds reading loopback stand-ins, driven
# through its TLS API and stream by a load generator in a second process (LOAD_TRADERS, LOAD_SUBSCRIBERS,
# LOAD_CRYPTO_RATE, LOAD_RWA_MOVES and LOAD_SOURCE_DELAY_MS size it as they do there). The node runs the
# launch configuration's storage (SOAK_STORAGE, 1 unless 0): `[daily_snapshots]` and `[archive]` with a
# `local_window`, and no `[snapshots]`, which the log's window cannot run beside:
# - its log's ranges and the tails of its checkpoints, and each daily snapshot, go to a loopback stand-in for the
#   bucket (held in this process's memory, which the memory bound allows for) below a stand-in vault that
#   finalizes each checkpoint 20 s after the node reports it final;
# - the log's local window removes the segments the bucket holds that are older than a day and that the newest daily
#   snapshots do not need, and the snapshots' window the host's snapshots older than a day and a half;
# - a restart goes on from the newest daily snapshot and the log after it, so the log before it may be off the host,
#   and after the run the segments off the host are read back from the bucket (`archive::restore_whole`) before the
#   whole log replays from the book.
# A daily snapshot is taken at the first checkpoint of each day of sequenced time, and the windows count in days. A
# run's real clock crosses at most one UTC midnight in eight hours, which prunes nothing, so the day is
# SOAK_DAY_MINUTES (twice the report interval, 120 minutes in a full run: four snapshots), counted by a setting
# only a test build has (`daily_snapshots::shorten_day`); SOAK_DAY_MINUTES=1440 keeps the UTC day, and then a run
# of under three days has pruned nothing and is not judged on pruning. Each interval's report names what the
# bucket holds (ranges, tails and snapshots, and their bytes), what the host has let go (segments of the log and
# snapshots) and the bytes the log and the snapshots hold on the host; an interval fails when the bucket falls
# more than six ranges behind or a storage task ends, and the run when it was a day long and the bucket holds no
# range or snapshot, three days long and nothing left the host, or none of its restarts from the third day on found
# the log's older segments off the host. The archive's and the snapshots' alerts fail an interval as any alert
# does. SOAK_STORAGE=0 runs the node as one with `[snapshots]` instead: a recovery cache every SOAK_SNAPSHOT_SECS
# (300) of sequenced time, its history index, and a restart from the newest cache.
# It runs SOAK_HOURS (8) at SOAK_MULTIPLE (10) times launch flow in intervals of
# SOAK_REPORT_MINUTES (60), each judged in windows of SOAK_WINDOW_SECS (120):
# - every SOAK_FAULT_MINUTES (20) one of #867's faults, in turn: a stalled venue, a dropped one, a slow
#   link, a quiet product, both venues down, and trusted time down;
# (SOAK_SHORT=1, which CI sets, is for a run that tests the harness: its events come too close together
# to leave a quiet window and its intervals are too short for memory to settle, so neither fails one.)
# SOAK_KNOWN_ALERTS=<names> lists alerts the run is told to expect whatever it does, each a known issue: an
# alert outside it fails an interval unless an event that explains its name was under way.
# - every SOAK_RESTART_HOURS (2, 0 for none) the node stops and starts again from its newest daily snapshot (its
#   newest cache with SOAK_STORAGE=0), on the same API address, the generator's clients reconnecting.
# After each interval it prints the load scenario's figures, then a `SOAK` report: the windows judged
# against #904's launch bar, those a fault, a restart or the start excuses apart; memory (the physical
# footprint on macOS, which counts compressed pages, resident memory elsewhere; the report names it)
# against the first interval's, allowed the growth of the newest cache (the newest daily snapshot's size
# with the storage on), which the state's size sets, and the stand-in bucket's bytes; the bytes the log and
# the caches (with their history index), or the daily snapshots, hold on disk and the log's growth a command; the stats worker's lag; each fault's and restart's recovery; and the alerts no event
# excused. It passes if every interval does (`verdict PASS`), every market is priced again within
# #867's deadlines after each fault, and the log replays to its roots from the rebuilt book at the end.
# It does not run the marks cache or the alert outbox, so its report holds no figure for their disk.
# Restarts alternate a drain, whose newest cache (SOAK_STORAGE=0) is the final checkpoint's, with a crash: the
# node is dropped with no drain and no final checkpoint, so the start replays the log past its newest cache, or
# its newest daily snapshot.
#
# What a full run needs, extrapolated from nine minutes at 10,000 accounts (not yet measured at 100,000):
# - disk: the log grows about 2 KB a command at about 100 commands a second, so 5 to 10 GB over 8 hours;
#   the caches and the history index grew about 12 KB a command (#1114), so about 40 GB more, and each
#   cache is ten times larger at 100,000 accounts. Give SOAK_DIR 80 GB free and watch the report's
#   `disk_bytes` lines: stop the run if the index outgrows that.
# - memory: about 150 MiB resident at 10,000 accounts, peaking near 450 MiB at a restart, so about 1.5 GiB
#   at 100,000 and 3 to 4 GiB at a restart, with the generator's process beside it.
# - with the storage on (the default), the disk holds the log less what the windows remove and the daily snapshots
#   (about 30 MB each at 10,000 accounts, so about 300 MB at 100,000, a day and a half of them), and this process
#   holds the stand-in bucket in memory: the log's ranges (about a sixth of the log's bytes), the daily snapshots
#   (four in eight hours, so about 1.3 GB at 100,000 accounts) and the tails of the checkpoints the vault has not
#   finalized, which the stand-in drops once it has, as a bucket's lifecycle rule would. Add about 2 GiB of memory.
# - time: the book takes minutes to build, then SOAK_HOURS, then the replay of the whole log from the book.
#
# The rotation leg runs `dev-short-turns.sh` again and again for the hours: three members on
# development-only five-minute turns, real nodes on Anvil handing the keys over at each boundary, a
# member promoted past a killed incumbent, and each pass's log replayed across the change of signer
# (about twenty minutes a pass). Those nodes start from genesis and price one market from a loopback
# stand-in, so the leg carries no generator's load: a node started as a process cannot take over the
# in-process launch book. It needs Anvil, Forge, and installed Soldeer dependencies.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
rotation=0
hours=
for arg in "$@"; do
  case "$arg" in
    --rotation) rotation=1 ;;
    -*) echo "usage: $0 [--rotation] [hours]" >&2; exit 2 ;;
    *) hours="$arg" ;;
  esac
done
if [[ "$rotation" == 1 ]]; then
  # Each pass is the whole scenario on a fresh Anvil chain: a pass that fails ends the leg.
  awk -v hours="${hours:-2}" 'BEGIN { exit !(hours ~ /^[0-9.]+$/ && hours > 0) }' || {
    echo "the rotation leg's hours is a positive number, not '${hours:-2}'" >&2
    exit 2
  }
  end=$(( $(date +%s) + $(awk -v hours="${hours:-2}" 'BEGIN { printf "%d", hours * 3600 }') ))
  pass=0
  while (( $(date +%s) < end )); do
    pass=$(( pass + 1 ))
    echo "SOAK rotation pass $pass begins"
    "$root/scripts/dev-short-turns.sh"
    echo "SOAK rotation pass $pass passed"
  done
  (( pass > 0 )) || { echo "the rotation leg ran no pass" >&2; exit 1; }
  echo "SOAK rotation: $pass passes in ${hours:-2} hours, each handing the keys over at every boundary"
  exit 0
fi
if [[ -n "$hours" ]]; then
  export SOAK_HOURS="$hours"
fi
test=load_scenario::soak::dev_scenario_soak
cargo_test=(cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib "$test" -- --ignored --exact)
# A renamed or moved test would otherwise pass by running nothing. The whole list is read before it is
# matched: `grep -q` closing the pipe at its first match fails the list's writer under pipefail.
listed="$(CARGO_INCREMENTAL=0 "${cargo_test[@]}" --list)"
grep -qx "$test: test" <<<"$listed" || {
  echo "the dev scenario test $test was not found" >&2
  exit 1
}
CARGO_INCREMENTAL=0 "${cargo_test[@]}" --nocapture
