#!/usr/bin/env bash
# The log tail's dev scenario, on a dev node with isolated temporary state, a loopback stand-in for the bucket behind
# TLS from a test CA and a stand-in vault that takes the node's checkpoints and finalizes each one's predecessor:
#   1. the node's uploader puts each whole range below the vault's finalized checkpoint in the bucket, and the tail of
#      each new checkpoint, from the first range the bucket lacks to the checkpoint's own, before the vault finalizes
#      it;
#   2. a copy that parts from the vault's history after an earlier checkpoint, as a returning operator's does, uploads
#      its own tail under another key, and every object already in the bucket stays as it was;
#   3. with the node, the copy and their uploaders stopped and no node or mirror serving the log,
#      `engine exit-proof --log <the log's URL in the bucket> --seq <latestSeq> --state-root <latestRoot>` prints the
#      proofs the node's disk gives, for an account and the house;
#   4. `engine replay` from the bucket alone reaches the vault's root, through the node's commands and checkpoints.
# Passes with `PASS: the bucket alone holds the log through the vault's latest checkpoint`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
test=archive::read::tail::scenario::dev_scenario_log_tail
output=$(cargo test -p engine-node --lib --locked "$test" -- --exact --nocapture 2>&1) || { echo "$output"; exit 1; }
echo "$output"
# A renamed or moved test would otherwise pass by running nothing.
grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test did not run" >&2; exit 1; }
