#!/usr/bin/env bash
# The node load dev scenario: the launch book of 100k accounts on the production genesis's 34 markets
# taken over by a node in one process, its feeds reading loopback stand-ins for every connection the
# production template opens, driven through its TLS API and WebSocket stream by a load generator in a
# second process: signed market and limit orders, cancels, cancel-alls and close-alls from 2,000
# traders under session keys, through the API's trusted PROXY protocol peer, and every stream
# subscriber the template allows, half signed. It runs launch flow, then ten times it, two minutes
# each, and with --ramp a ramp from twenty times launch flow, doubling each minute, until every budget
# has broken or the sequencer is saturated, then prints the load at which each budget first broke.
# Each leg prints order-to-fill and price-to-stamp at p50, p99 and p999, the sequencer's
# batch time, stream lag, the sequencer's busy share, and the process's CPU and resident memory,
# against the launch bar; the run's log then replays to its roots. It fails if either measured leg
# breaks the bar, with a loaded runner's tolerance, or did not measure what it claims: a request
# refused, rejected or unanswered, a subscriber that took no frame or lost its connection, or no
# fill or stamped price.
#
# Needs only Cargo: the venues and the screening source are loopback stand-ins, on free ports, with
# private temporary data. The environment sizes the run: LOAD_ACCOUNTS (100000), LOAD_TRADERS (2000),
# LOAD_SUBSCRIBERS (256), LOAD_LEG_SECS (120), LOAD_CRYPTO_RATE (each crypto market's price commands
# a second, the median `dev-venue-load.sh --soak` prints) and LOAD_RWA_MOVES (each other market's mid
# moves a second on each of its connections). CI runs it at 10,000 accounts.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
if [[ "${1:-}" == "--ramp" ]]; then
  export LOAD_RAMP=1
fi
test=load_scenario::dev_scenario_load
cargo_test=(cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib "$test" -- --ignored --exact)
# A renamed or moved test would otherwise pass by running nothing.
CARGO_INCREMENTAL=0 "${cargo_test[@]}" --list | grep -qx "$test: test" || {
  echo "the dev scenario test $test was not found" >&2
  exit 1
}
CARGO_INCREMENTAL=0 "${cargo_test[@]}" --nocapture
