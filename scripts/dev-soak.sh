#!/usr/bin/env bash
# The soak dev scenario (#909): the node load scenario (`dev-load.sh`) run for hours at ten times
# launch flow, on a node that publishes recovery caches, with #867's faults and restarts in it.
#
#   ./scripts/dev-soak.sh [hours]               8 hours unless given
#   ./scripts/dev-soak.sh --rotation [hours]    the rotation leg instead, 2 hours unless given
#
# By hand before each release, in a quiet window, under `nice`, never at 100,000 accounts on a
# machine that is also doing other work, and with the report kept for the release notes
# (`ops/runbooks/deploy-and-rollback.md`):
#   SOAK_DIR=<a directory on a disk with room> nice ./scripts/dev-soak.sh 2>&1 | tee soak-report.txt
# Name SOAK_DIR: the log of a full run is large (see the report's `disk_bytes` lines), and without it
# the run writes under the temporary directory and removes it afterwards. With it the data is kept;
# delete it by its exact path once the report is saved.
#
# The run is the load scenario's: the launch book of LOAD_ACCOUNTS (100000) accounts on the production
# genesis's 34 markets taken over by a node in one process, its feeds reading loopback stand-ins, driven
# through its TLS API and stream by a load generator in a second process (LOAD_TRADERS, LOAD_SUBSCRIBERS,
# LOAD_CRYPTO_RATE, LOAD_RWA_MOVES and LOAD_SOURCE_DELAY_MS size it as they do there). The node
# publishes a recovery cache every SOAK_SNAPSHOT_SECS (300) of sequenced time, as one with `[snapshots]`
# does. It runs SOAK_HOURS (8) at SOAK_MULTIPLE (10) times launch flow in intervals of
# SOAK_REPORT_MINUTES (60), each judged in windows of SOAK_WINDOW_SECS (120):
# - every SOAK_FAULT_MINUTES (20) one of #867's faults, in turn: a stalled venue, a dropped one, a slow
#   link, a quiet product, both venues down, and trusted time down;
# (SOAK_SHORT=1, which CI sets, is for a run that tests the harness: its events come too close together
# to leave a quiet window and its intervals are too short for memory to settle, so neither fails one.)
# SOAK_KNOWN_ALERTS=<names> lists alerts the run is told to expect whatever it does (CI's lists
# snapshot_write_failed, #1114): an alert outside it fails an interval unless an event that explains its name
# was under way.
# - every SOAK_RESTART_HOURS (2, 0 for none) the node stops and starts again from its newest cache, on
#   the same API address, the generator's clients reconnecting.
# After each interval it prints the load scenario's figures, then a `SOAK` report: the windows judged
# against #904's launch bar, those a fault, a restart or the start excuses apart; resident memory
# against the first interval's, allowed the growth of the newest cache, which the state's size sets;
# the bytes the log and the caches (with their history index) hold on disk and the log's growth a
# command; the stats worker's lag; each fault's and restart's recovery; and the alerts no event
# excused. It passes if every interval does (`verdict PASS`), every market is priced again within
# #867's deadlines after each fault, and the log replays to its roots from the rebuilt book at the end.
# It does not run the marks cache or the alert outbox, so its report holds no figure for their disk.
# Restarts alternate a drain, whose newest cache is the final checkpoint's, with a crash: the node is
# dropped with no drain and no final checkpoint, so the start replays the log past its newest cache.
#
# What a full run needs, extrapolated from nine minutes at 10,000 accounts (not yet measured at 100,000):
# - disk: the log grows about 2 KB a command at about 100 commands a second, so 5 to 10 GB over 8 hours;
#   the caches and the history index grew about 12 KB a command (#1114), so about 40 GB more, and each
#   cache is ten times larger at 100,000 accounts. Give SOAK_DIR 80 GB free and watch the report's
#   `disk_bytes` lines: stop the run if the index outgrows that.
# - memory: about 150 MiB resident at 10,000 accounts, peaking near 450 MiB at a restart, so about 1.5 GiB
#   at 100,000 and 3 to 4 GiB at a restart, with the generator's process beside it.
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
