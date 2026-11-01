#!/usr/bin/env bash
# Runs the standby operator scenario against an isolated Anvil: A runs as the operator in effect
# with rotation on and payouts paused, B follows A's real stream into a copy, A fails, the liveness
# rule promotes B, the timelock installs only B's registered enclave, B resumes at A's last
# vault-accepted checkpoint, the paused authorization is reissued under B's signer epoch and pays
# once, and the log replays across the change of signer. A second promotion with no qualifying
# standby reverts.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys
# are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  sequencer::withdrawal_scenario::dev_scenario_a_standby_takes_over_from_a_failed_operator \
  -- --ignored --exact
