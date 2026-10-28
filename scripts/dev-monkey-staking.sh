#!/usr/bin/env bash
# Runs the MONKEY staking and USDC distributions scenario against an isolated Anvil: staking
# through the Staking contract mirrored into the engine, an epoch above the liquidity floor paying
# stakers pro-rata in USDC against posted checkpoint roots, an unstake through the cooldown, and a
# second epoch below the floor paying nothing.
# Needs Anvil, Forge, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  checkpoints::staking_scenario::dev_scenario_staking_pays_stakers_above_the_floor_and_nothing_below_it \
  -- --ignored --exact
