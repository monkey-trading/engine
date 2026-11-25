#!/usr/bin/env bash
# The log archive's dev scenario, on a dev node with isolated temporary state, a loopback stand-in for the bucket
# behind TLS from a test CA, which checks each upload's signature, and a stand-in vault:
#   1. each whole range of the log goes up compressed, and reads back as the node's copy of it, once the vault has
#      finalized a checkpoint past it, and not before;
#   2. with the bucket down, the node keeps sequencing, its waiting ranges grow and `archive_upload_lagging` fires;
#      with it back, the node catches up and the alert clears;
#   3. a member's copy of the log, cut into other segments, uploads the same ranges and changes no object;
#   4. the node restarts, and its uploader resumes at the first missing range, reading back only the one before;
#   5. a range whose object reads back other than the node's copy raises `archive_object_mismatch`, and nothing
#      past it goes up while the node sequences on.
# Passes with `PASS: the bucket holds every finalized range ...`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=archive::scenario::dev_scenario_log_archive
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact --nocapture 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
