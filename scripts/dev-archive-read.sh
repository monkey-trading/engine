#!/usr/bin/env bash
# The log archive read's dev scenario, on a dev node with isolated temporary state, a loopback stand-in for the bucket
# behind TLS from a test CA and a stand-in vault:
#   1. the node writes flow, and its uploader puts each whole range below the vault's finalized checkpoint in the
#      bucket;
#   2. the segments the bucket's first ranges hold leave the node's disk, and `GET /v1/archive/<segment>` answers `308`
#      to the object of the range holding its first seq, from which the segment rebuilds to its manifest line;
#   3. `engine replay --log <the node's URL>` reaches the roots, commands and signers of the local log's replay;
#   4. a member with an empty copy backfills it, its older segments from the bucket and its newest from the node;
#   5. the bucket serves other bytes for an object: the backfill takes its segments from the node, and `engine replay`
#      refuses a segment the node no longer holds, naming it;
#   6. `engine exit-proof --log <the node's URL>` prints the proofs it prints from the local log.
# Passes with `PASS: the whole log reads from the bucket and the node ...`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=archive::read::scenario::dev_scenario_archive_read
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact --nocapture 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
