#!/usr/bin/env bash
# Runs the checkpoint scenario against an isolated Anvil: posting, strict replay, edited-signature
# refusal, startup truncation refusal, and withdrawal pause while a direct transfer stays unbooked
# or a collateral shortfall persists.
# Needs Python3, Anvil, Forge, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  checkpoints::scenario::dev_scenario_posts_replays_refuses_truncation_and_pauses_shortfall -- --ignored --exact
# A scenario transaction sent beside another shares its block, so Anvil never leaves one pending.
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  checkpoints::scenario::transact::tests::two_transactions_sent_together_share_a_block_instead_of_one_waiting_unmined \
  -- --ignored --exact
# The production gate's accounting capture must stay bounded as accounts and deposit refs grow.
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  sequencer::checkpoints::accounting_tests::gated_work_capture_scaling -- --ignored --exact --nocapture
