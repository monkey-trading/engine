#!/usr/bin/env bash
# The recovery and handover dev scenario at scale: a long log of launch flow on the launch book of
# 100k accounts on the production genesis's 34 markets, written through a node's sequencer on
# accelerated sequenced time (30 days of launch flow, then 3 days of ten times it, each market priced
# at its live rate while its venue is open, each trader ordering only in open markets), then each way a
# node comes back on it. The node pairs recovery caches as one with [snapshots] does, a capture every
# five minutes of sequenced time, and takes a cache five minutes of flow before a hard stop. On that
# log: `engine replay`'s replay, a restart from the cache to its first command through the node's own
# recovery and start checks with the head posted an hour before the log's end, a node's recovery by
# full replay to its first command, a member's replica build and the history index's catch-up, and
# tail handovers grown under launch flow to about 1 MiB, 64 MiB and 1 MiB under the handover's bound,
# each ending with the member's start from its replica. It prints each day's commands and bytes; each
# capture's hold on the sequencer against one command's 500 ms; the 30-day full replay projected from
# the markets' calendars and the replay's rate, against 30 minutes; the restart from the cache against
# 2 minutes, with its parts and the start's whole-log reads projected to 30 days; each switch against
# the daily rotation's 1 s, with whether a predecessor would admit its tail; and the largest tail whose
# switch fits. It fails if a day of flow priced or filled nothing or rejected over 1% of its requests,
# the flow closes no MONKEY emission period or staking epoch, a replay does not reach the log's roots,
# the restart does not resume from the cache, the history index does not take the log's last command,
# or a handover does not take its tail.
#
# Needs only Cargo; private temporary data under TMPDIR. The log needs disk: about the bytes a day it
# prints, times the days run. The environment sizes the run: RECOVERY_ACCOUNTS (100000),
# RECOVERY_TRADERS (2000), RECOVERY_START_SECS (0: the flow starts at the book's clock, Monday 15:00
# UTC), RECOVERY_LAUNCH_SECS (2592000), RECOVERY_TEN_TIMES_SECS (259200), RECOVERY_CAPTURE_SECS (300),
# RECOVERY_SUFFIX_SECS (300), RECOVERY_TAILS (bytes, comma-separated) and RECOVERY_REPLAY_WORKERS (the
# threads that check command seals ahead of each replay that checks every seal, the node's included: one
# per core). CI runs it at 10,000 accounts and twenty-one minutes of flow across the US shares' close,
# which the first emission period and staking epoch end in.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
test=sequencer::recovery_scale::dev_scenario_recovery_scale
cargo_test=(cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib "$test" -- --ignored --exact)
# A renamed or moved test would otherwise pass by running nothing.
CARGO_INCREMENTAL=0 "${cargo_test[@]}" --list | grep -qx "$test: test" || {
  echo "the dev scenario test $test was not found" >&2
  exit 1
}
CARGO_INCREMENTAL=0 "${cargo_test[@]}" --nocapture
