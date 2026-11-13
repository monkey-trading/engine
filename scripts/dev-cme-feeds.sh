#!/usr/bin/env bash
# The CME Group feed's dev scenario: a loopback stand-in for CME's OAuth token endpoint and its market
# data WebSocket, each behind TLS a test CA certifies, issues twelve-second bearer tokens, answers each
# login after the first a second and a half late, and restates US 500's contracts' tops of book every
# 500 ms beside a loopback Massive, and the sequencer prices US 500 from the two. The feed refreshes
# each token before it expires and switches to a connection opened beside the old one on the next,
# so US 500 fills throughout two rollovers and CME's quotes never pause past 900 ms. Once the endpoint
# refuses, venue_token_failed and then venue_feed_down fire, US 500 goes unpriced, no credential or
# token reaches a log line, and the written log replays. Needs only Cargo: the stand-ins are local,
# so no CME account or key is used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  feeds::tests::cme::dev_scenario_cme_feeds -- --ignored --exact --nocapture
