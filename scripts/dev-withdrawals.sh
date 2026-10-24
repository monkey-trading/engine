#!/usr/bin/env bash
# Runs the actual local TLS API/stream, EVM custody quorum, checkpoint poster and Vault withdrawal
# scenario, including SIGKILL before approval logging and after durable authorization delivery.
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
  tail -n 45 "$SCENARIO_LOG_DIR/scenario.log" >&2
  exit 1
fi
grep -E 'PASS:|^test result:' "$SCENARIO_LOG_DIR/scenario.log"
