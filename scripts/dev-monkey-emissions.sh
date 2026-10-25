#!/usr/bin/env bash
# Runs the MONKEY emissions scenario against an isolated Anvil: pro-rata allocation of fixed
# ten-minute budgets, cancelled empty periods, a restart across a period's finalisation, the first
# halving, and claims against posted checkpoint roots that mint each entitlement once.
# Needs Anvil, Forge, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  checkpoints::emissions_scenario::dev_scenario_emissions_share_fixed_budgets_halve_survive_restart_and_claim_once \
  -- --ignored --exact
