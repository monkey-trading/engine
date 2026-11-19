#!/usr/bin/env bash
# Portfolio margin on a dev node through the signed order routes, the signed admin route over TLS and the
# account read, on deterministic time with isolated temporary state. Two tests run.
#
# The read (config/dev.toml's fixture group, BTC-USD and ETH-USD): account A opens $1M long BTC-USD and
# $1M short ETH-USD, account B $1M long BTC-USD alone, and each read is verified as `engine verify-response`
# verifies it. A's matched exposure takes the group's credit, leaving its portfolio requirement below its
# per-position requirement and above the group's floor share of it; B's matches nothing and equals its
# per-position requirement; after BTC-USD moves 2% both reads follow the new marks; and once A closes its
# ETH-USD leg its portfolio requirement equals its per-position requirement.
#
# Close-outs (V81), with a crypto group and a metals group, XAU-USD with XAG-USD listed for the scenario,
# in the genesis parameters:
#  1. A opens a BTC-USD/ETH-USD pair in alternating slices and AAPL, which is in no group; B opens the
#     BTC-USD leg alone.
#  2. ETH-USD rises until A's equity is between the close-out level of its portfolio requirement and that of
#     its per-position requirement: A is not closed out.
#  3. The basis widens a little further, A still holds, then AAPL falls: A's close-out closes AAPL first,
#     then the BTC-USD/ETH-USD pair together, BTC-USD before ETH-USD; no leg closes alone.
#  4. E holds XAU-USD/XAG-USD, a group of two markets on one weekly calendar with no weekend margin, to
#     Friday's close above the close-out level of its portfolio requirement and below its per-position
#     requirement in full: the metals keep no weekend gap, so nothing more is asked of E, and E holds the pair
#     past the close.
#  5. C's pair holds above its portfolio level; ETH-USD's quotes go stale and BTC-USD falls below the level:
#     ETH-USD waits at its last mark and BTC-USD closes alone.
#  6. D closes a pair with a close-all while below the level after the first entry: both entries fill
#     before any close-out check, and none is a close-out.
#  7. A higher residual shock is announced: a set-params before its time is refused and one after applies; a
#     lower shock, a shock past its bound and a changed loading are refused at the announcement.
#  8. Both metals are relisted with a holiday added in one command; one member relisted alone with another
#     holiday is refused.
#  9. The public log replays to the same state, and a restart recovers the root it replays to.
# Steps 4, 5 and 6 run in the order 5, 6, 4 on the clock: the pair holders trade on Tuesday, and the
# metals close on Friday.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
tests=(
  api::access_tests::portfolio_margin::dev_scenario_a_hedged_pair_reads_a_portfolio_requirement_below_its_bands
  api::access_tests::portfolio_margin::closeouts::dev_scenario_close_outs_read_the_portfolio_requirement
)
for test in "${tests[@]}"; do
  output=$(cargo test -p engine-node --lib --locked "$test" -- --exact 2>&1) || { echo "$output"; exit 1; }
  echo "$output"
  # A renamed or moved test would otherwise pass by running nothing.
  grep -q '^test result: ok. 1 passed' <<<"$output" || { echo "the dev scenario test $test did not run" >&2; exit 1; }
done
