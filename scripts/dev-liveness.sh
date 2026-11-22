#!/usr/bin/env bash
# Runs the minute-scale liveness scenario (#595) on an isolated Anvil chain whose blocks stand in for
# Arbitrum's (a ChainArbSys stand-in etched at 0x64), with the enclaves' trusted time on the chain's
# clock. A, B and C register with standby keys the vault's signer vouches for, a close adopts them,
# and a turn is drawn; its member, the operator, runs the real head poster and liveness rule, asking
# the other members' real member links for their copies, and the others run the real liveness caller.
# It checks: v2 heads land at the confirmed seqs and the first latches the turn; with the operator's
# relayer silenced the members relay its heads and nothing is promoted; through a benchmark-sync hold
# and then a trusted-time halt in which no enclave gets a sample past the grace, each past the detection
# window, held heads land and nothing is promoted; through a paused chain the operator stops by its own clock and resumes on its first read
# after it, with no promotion; with the operator's chain reads blocked, the members witness each slot,
# the operator fences before the earliest promotion, and the promotion moves the vault's signer to the
# next member's standby key, whose seal opens as that promotion's; and a reorg that removes the
# promotion reads as unsettled until a block passes it, then as gone.
# Then the promotion on real nodes: A, B and C run the node binary's own start as child processes on a
# chain running on true time, the turn in effect drawn for A. A holds it, posting heads at the seqs B
# and C confirm, takes a deposit, fills an order and authorizes a withdrawal, and is killed; the
# callers promote the next member, whose node starts from its copy under its standby key, fills an
# order within its start budget (printed), posts its first checkpoint once the promotion is final,
# vouches for its replacement standby key with its own signer key, and reissues A's authorization, which pays once while A's reverts, and its log replays across the change
# of signer. Together they take about thirty minutes.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)

# Each member's node log is in a scratch directory a CI runner discards; with SCENARIO_LOG_DIR set they
# go there, where the job uploads them when it fails.
if [[ -n "${SCENARIO_LOG_DIR:-}" ]]; then
  mkdir -p "$SCENARIO_LOG_DIR"
  export TMPDIR="$SCENARIO_LOG_DIR"
fi
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib -- \
  --ignored --exact --nocapture \
  custody::tests::election_scenario::liveness::dev_scenario_a_failed_operator_is_promoted_past_within_minutes
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib -- \
  --ignored --exact --nocapture \
  custody::tests::election_scenario::liveness::promoted::dev_scenario_a_promoted_member_trades_within_its_start_budget
