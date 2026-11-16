#!/usr/bin/env bash
# The venue load dev scenario: BTC-USD and ETH-USD priced from loopback Coinbase and Kraken stand-ins
# at the production template's feed timings, with the node's monitor watching. The stand-ins send
# what the venues send: Coinbase's full books (4 MB for BTC-USD, 2 MB for ETH-USD), both venues'
# update rates, and pongs in stream order behind what was sent before. Links between the feeds and
# the stand-ins inject each fault in turn: a stalled and a dropped Coinbase connection, a slow link,
# ETH-USD going quiet, ETH-USD served ten minutes behind as on 16 November 2026, an outage of both
# venues, and an outage of trusted time. The node alerts while a market is unpriced, and every market
# is priced again within 10 s of its venues streaming again, or within reconnect_max more after an
# outage of both venues or of trusted time. Needs only Cargo: the venues are local stand-ins. About
# five minutes.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  feeds::tests::venue_load::dev_scenario_venue_load -- --ignored --exact --nocapture
