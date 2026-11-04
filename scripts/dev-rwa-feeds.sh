#!/usr/bin/env bash
# The RWA price feeds' dev scenario: loopback Massive and TraderMade vendors, the sources reached over
# TLS that price these markets, price EUR-USD from both, and US 500 and gold from front futures
# contracts and Apple from Massive alone, through a sequencer at the latest protocol. Apple's market
# opens a few seconds in and US 500 rolls onto its next contract a few seconds later; the written log
# then replays. Needs only Cargo: the vendors are local stand-ins, so no vendor account or key is used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  feeds::tests::rwa::dev_scenario_rwa_price_feeds -- --ignored --exact --nocapture
