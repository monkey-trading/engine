#!/usr/bin/env bash
# A new member's start from a daily snapshot, on deterministic time with isolated temporary state: a node
# writing launch flow on a small book across a UTC midnight has a signed snapshot, carrying the replay state, at
# its first checkpoint and at its first past midnight; a member with an empty copy backfills the node's archive over HTTP, fetches the
# newest snapshot its copy holds, and builds its replica from it with no whole replay, to what a whole replay of
# the copy gives; it takes the node's turn through the tail handover and stamps its first command; a member whose
# in-office member's turns began at midnight takes the snapshot before them; and a snapshot whose state its signer
# did not vouch for is passed over with Warning `snapshot_start_refused`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=sequencer::recovery_scale::member_snapshot::dev_scenario_member_snapshot_start
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact --nocapture 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
