#!/usr/bin/env bash
# Runs the operator faults scenario (#597) on an isolated Anvil chain: A, B and C register with standby
# keys the vault's signer vouches for, a close adopts them, and a turn is drawn for B. The node, the
# enclave holding the vault's signing key, takes B's turn; its host stops it, keeps a copy of its log,
# lets it sequence on, then restarts it from the copy, so it sequences a second history over the same
# seqs. The enclave's key signs an engine.log-head.v2 head of each at one seq, binding B's assignment,
# the first lands on LogHeads as B's heartbeat, and an account with no role submits both to the real
# FaultProof contract.
# It checks: FaultProven names B and the custody watcher mirrors it; the next distribution cuts B's part
# of the operator's share by the haircut's average, starting at the penalty, with the withheld part
# going to stakers; the next epoch's cut is smaller as the haircut falls linearly, and once it has
# decayed B's next turn is paid whole; and after a promotion moves the vault's signer to a standby key
# in a new signer epoch, a head the new key signs paired with B's proves nothing under either epoch, and
# two heads the new key signs for B's assignment, which left office before that epoch began, name no one.
# Needs Anvil, Forge, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib -- \
  --ignored --exact --nocapture \
  custody::tests::election_scenario::faults::dev_scenario_a_member_that_signs_two_histories_has_its_share_cut
