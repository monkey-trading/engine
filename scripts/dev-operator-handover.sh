#!/usr/bin/env bash
# Runs the operator handover scenario against an isolated Anvil: A runs as the operator in effect
# with rotation on and authorizes a withdrawal, which pays before the election warp (the vault's
# 24-hour authorization lifetime runs at chain time, which the seven-day election notice outruns);
# an election names B and, past its effective time, B reads A's handover endpoint from the
# registry, fetches the published log, takes the keys and the final log segments, and continues at
# the seq after A's final checkpoint while the vault accepts B's checkpoints; A exits; A's stale
# sealed copy refuses to open; a re-election of A hands the keys back; C, a third member following
# since before B's election, is elected and takes the whole log from A onto its set-aside copy, and
# a re-election hands it back to A the same way; then A upgrades with
# rotation on: its candidacy moves to a release the timelock approved, and that release takes the
# keys and the whole log from A and continues under the same signer.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys
# are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  sequencer::withdrawal_scenario::dev_scenario_an_elected_operator_takes_over \
  -- --ignored --exact
