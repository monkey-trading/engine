#!/usr/bin/env bash
# Runs the exit against the last posted root on Anvil: a real node with its TLS API trades and
# checkpoints, then stops; the proofs come from its public log alone, and the vault's exit starts,
# registers, refunds, pays and releases the remainder through the timelock.
# Needs Cargo, Forge, Anvil and pinned Soldeer dependencies. Uses no external server or real funds.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
if [[ -z "${SCENARIO_LOG_DIR:-}" ]]; then
  SCENARIO_LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/engine-exit.XXXXXX")"
fi
export SCENARIO_LOG_DIR
mkdir -p "$SCENARIO_LOG_DIR"
echo "Exit scenario artifacts: $SCENARIO_LOG_DIR"

if ! (cd "$root/contracts" && forge build) >"$SCENARIO_LOG_DIR/forge.log" 2>&1; then
  tail -n 30 "$SCENARIO_LOG_DIR/forge.log" >&2
  exit 1
fi
if ! cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  sequencer::withdrawal_scenario::dev_scenario_exit_against_the_last_posted_root -- \
  --ignored --exact --nocapture --test-threads=1 >"$SCENARIO_LOG_DIR/scenario.log" 2>&1; then
  grep -n -m 1 -A 16 -E 'panicked at|Error:|error:' "$SCENARIO_LOG_DIR/scenario.log" >&2 || true
  # A node child's own failure is only in its log, which a CI runner discards.
  for child in "$SCENARIO_LOG_DIR"/*/node.log; do
    [[ -f "$child" ]] || continue
    echo "--- last lines of $child" >&2
    tail -n 30 "$child" | cut -c 1-400 >&2
  done
  tail -n 45 "$SCENARIO_LOG_DIR/scenario.log" >&2
  exit 1
fi
grep -E 'PASS:|^test result:' "$SCENARIO_LOG_DIR/scenario.log"
