#!/usr/bin/env bash
# Runs the enclave key lifecycle on Anvil under the mock TEE and its stand-in for sealing: a first
# boot refused until the timelock makes its release current, then waiting for it to register the new
# key; a restart that keeps the key with a withdrawal outstanding; a refused start without the
# sealed file; an edited config that neither opens nor seals keys; an upgrade refused until the
# timelock makes it current and then handed the keys; the superseded release refused; the same
# release named again, refusing its older copy and taking the keys afresh; and a second copy that
# stops on the vault's checkpoint conflict. Needs Cargo, Forge, Anvil and pinned Soldeer
# dependencies. Uses no external server or real funds.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
if [[ -z "${SCENARIO_LOG_DIR:-}" ]]; then
  SCENARIO_LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/engine-key-lifecycle.XXXXXX")"
fi
export SCENARIO_LOG_DIR
mkdir -p "$SCENARIO_LOG_DIR"
echo "Key lifecycle scenario artifacts: $SCENARIO_LOG_DIR"

if ! (cd "$root/contracts" && forge build) >"$SCENARIO_LOG_DIR/forge.log" 2>&1; then
  tail -n 30 "$SCENARIO_LOG_DIR/forge.log" >&2
  exit 1
fi
if ! cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  sequencer::withdrawal_scenario::dev_scenario_key_lifecycle -- \
  --ignored --exact --nocapture --test-threads=1 >"$SCENARIO_LOG_DIR/scenario.log" 2>&1; then
  grep -n -m 1 -A 16 -E 'panicked at|Error:|error:' "$SCENARIO_LOG_DIR/scenario.log" >&2 || true
  # A node child's own failure is only in its log, which a CI runner discards.
  for child in "$SCENARIO_LOG_DIR"/key-lifecycle/*/node-*.log; do
    [[ -f "$child" ]] || continue
    echo "--- $child" >&2
    grep -n -m 5 -E ' ERROR |^Error:|panicked at' "$child" >&2 || true
    echo "--- last lines of $child" >&2
    tail -n 20 "$child" | cut -c 1-400 >&2
  done
  tail -n 45 "$SCENARIO_LOG_DIR/scenario.log" >&2
  exit 1
fi
grep -E 'PASS:|^test result:' "$SCENARIO_LOG_DIR/scenario.log"
