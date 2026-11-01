#!/usr/bin/env bash
# Runs the operator handover scenario against an isolated Anvil: A runs as the operator in effect
# with rotation on; an election names B and, past its effective time, B reads A's handover endpoint
# from the registry, fetches the published log, takes the keys and the final log segment, and
# continues at the seq after A's final checkpoint; A exits; a withdrawal A authorized before the
# handover pays; A's stale sealed copy refuses to open; a re-election of A hands the keys back.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys
# are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  sequencer::withdrawal_scenario::dev_scenario_an_elected_operator_takes_over \
  -- --ignored --exact
