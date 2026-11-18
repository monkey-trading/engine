#!/usr/bin/env bash
# The venue load dev scenario: BTC-USD and ETH-USD priced from loopback Coinbase and Kraken stand-ins
# at the production template's feed timings, with the node's monitor watching. The stand-ins send
# what the venues send: Coinbase's full books (4 MB for BTC-USD, 2 MB for ETH-USD), both venues'
# update rates, and pongs in stream order behind what was sent before. Links between the feeds and
# the stand-ins inject each fault in turn: a stalled and a dropped Coinbase connection, a slow link,
# ETH-USD going quiet, ETH-USD served ten minutes behind as on 16 November 2026, an outage of both
# venues, and an outage of trusted time. The node alerts while a market is unpriced, and every market
# is priced again within 10 s of its venues streaming again, or within reconnect_max more after an
# outage of both venues or of trusted time. Then, with a second connection to each venue, each
# connection is stalled and dropped in turn, and every market stays priced throughout. Needs only
# Cargo: the venues are local stand-ins. About six minutes.
#
# With --soak [minutes] it runs the manual soak instead, before each release and never in CI: the
# template's crypto markets, BTC-USD, ETH-USD and SOL-USD, from the live venues, over every connection
# the production template opens for them, for 30 minutes unless given, dropping each connection alone
# in turn, then all of them, every two minutes. No market is unpriced for longer than 10 s, through a
# drop or between them, and each time one is the soak prints for how long. At the end it prints each
# market's price commands a second, at the median second and in the busiest, the rate the node load
# scenario's stand-ins replay (dev-load.sh). Needs Cargo and the venues' public feeds.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
test="feeds::tests::venue_load::dev_scenario_venue_load"
if [[ "${1:-}" == "--soak" ]]; then
  export SOAK_MINUTES="${2:-30}"
  test="feeds::tests::venue_soak::soak_live_venues"
fi
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  "$test" -- --ignored --exact --nocapture
