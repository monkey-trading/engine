#!/usr/bin/env bash
# Runs the seed relay scenario on two isolated Anvil chains, one as Ethereum, whose built-in
# EIP-4788 contract records a stand-in beacon node's block roots, and one as Arbitrum, with a
# stand-in bridge that delivers the Inbox's tickets from the relay's aliased address. The node's
# relayer, built from a node config and reading each chain through three local RPC proxies, proves
# and relays three daily seeds, which the receiver records as their epochs' mixes; a relay proving
# another epoch's mix reverts on Ethereum, and a seed from another sender or for another epoch
# reverts on Arbitrum; with the beacon node stopped, relaying fails with `seed_relay_failed`, and
# once the next turn's draw deadline passes `seed_missing` fires and the turn draws from the
# fallback seed.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  seed_relay::tests::scenario::dev_scenario_three_daily_seeds_reach_arbitrum_and_a_missing_one_alerts \
  -- --ignored --exact
