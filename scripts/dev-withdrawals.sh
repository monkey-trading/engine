#!/usr/bin/env bash
# Runs the actual local TLS API/stream, EVM custody quorum, checkpoint poster and Vault withdrawal
# scenario, including SIGKILL before approval logging and after durable authorization delivery, and
# the guardian's pause and signer retirement through to the timelock's new epoch.
# Needs Cargo, Forge, Anvil and pinned Soldeer dependencies. Uses no external server or real funds.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=2
if [[ -z "${SCENARIO_LOG_DIR:-}" ]]; then
  SCENARIO_LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/engine-withdrawals.XXXXXX")"
fi
export SCENARIO_LOG_DIR
mkdir -p "$SCENARIO_LOG_DIR"
echo "Withdrawal scenario artifacts: $SCENARIO_LOG_DIR"

if ! (cd "$root/contracts" && forge build) >"$SCENARIO_LOG_DIR/forge.log" 2>&1; then
  tail -n 30 "$SCENARIO_LOG_DIR/forge.log" >&2
  exit 1
fi
if ! cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  sequencer::withdrawal_scenario::dev_scenario_signed_withdrawals_and_fifo -- \
  --ignored --exact --nocapture --test-threads=1 >"$SCENARIO_LOG_DIR/scenario.log" 2>&1; then
  grep -n -m 1 -A 16 -E 'panicked at|Error:|error:' "$SCENARIO_LOG_DIR/scenario.log" >&2 || true
  # A node child's own failure is only in its log, which a CI runner discards.
  for child in "$SCENARIO_LOG_DIR"/*/node.log; do
    [[ -f "$child" ]] || continue
    echo "--- $child" >&2
    grep -n -m 5 -E ' ERROR |^Error:|panicked at' "$child" >&2 || true
    echo "--- checkpoints in $(dirname "$child")/log (seq, root, parent seq)" >&2
    cat "$(dirname "$child")"/log/[0-9]*.jsonl 2>/dev/null \
      | jq -c 'select(.entry == "checkpoint") | [.seq, .root[0:18], .parent.seq]' 2>/dev/null | tr '\n' ' ' >&2 || true
    echo >&2
    echo "--- last lines of $child" >&2
    tail -n 30 "$child" | cut -c 1-400 >&2
  done
  tail -n 45 "$SCENARIO_LOG_DIR/scenario.log" >&2
  exit 1
fi
grep -E 'PASS:|^test result:' "$SCENARIO_LOG_DIR/scenario.log"
