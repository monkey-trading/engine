#!/usr/bin/env bash
# The US share markets' dev scenario: loopback stand-ins for Massive's stocks feed and Alpaca's
# consolidated (SIP) feed, each behind TLS a test CA certifies, quote Apple while its market is
# closed. A relisting opens it two seconds on, and only quotes after the open, from both sources, fill
# a resting limit at their index. Alpaca stopping (venue_feed_down), and then Massive, each leaves
# Apple unpriced until it is back; no key ID or secret key reaches a log line, and the written log
# replays. Needs only Cargo: the stand-ins are local, so no vendor account or key is used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  feeds::tests::shares::dev_scenario_share_feeds -- --ignored --exact --nocapture
