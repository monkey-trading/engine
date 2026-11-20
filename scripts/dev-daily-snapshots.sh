#!/usr/bin/env bash
# Daily snapshots and a replay in ranges from them, on deterministic time with isolated temporary state:
# a node writing launch flow on a small book across a UTC midnight publishes a snapshot at its first
# checkpoint and at its first past midnight; the log replays in three ranges from them, two at once, to
# what the whole log replays to, under a pinned and an attested signer; a snapshot past the log's last
# checkpoint starts no range; a snapshot whose index of resting orders lost an order replays the same, and
# one whose balance or checkpoint the log's signed root does not vouch for is refused.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=sequencer::recovery_scale::daily::dev_scenario_daily_snapshots
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
