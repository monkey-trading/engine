#!/usr/bin/env bash
# Runs the minute-scale liveness scenario (#595) on an isolated Anvil chain whose blocks stand in for
# Arbitrum's (a ChainArbSys stand-in etched at 0x64), with the enclaves' trusted time on the chain's
# clock. A, B and C register with standby keys the vault's signer vouches for, a close adopts them,
# and a turn is drawn; its member, the operator, runs the real head poster and liveness rule, asking
# the other members' real member links for their copies, and the others run the real liveness caller.
# It checks: v2 heads land at the confirmed seqs and the first latches the turn; with the operator's
# relayer silenced the members relay its heads and nothing is promoted; through a trusted-time halt
# past the detection window held heads land and nothing is promoted; through a paused chain the
# operator stops by its own clock and resumes on its first read after it, with no promotion; and once
# the operator fails, the members witness each slot, the operator fences before the earliest
# promotion, and the promotion moves the vault's signer to the next member's standby key, whose seal
# opens as that promotion's. It takes about ten minutes of wall time.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib -- \
  --ignored --exact --nocapture \
  custody::tests::election_scenario::liveness::dev_scenario_a_failed_operator_is_promoted_past_within_minutes
