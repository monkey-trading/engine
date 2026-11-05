#!/usr/bin/env bash
# Runs the daily rotation scenario on two isolated Anvil chains, with time warped by the chains and
# the node's test clock together. On the custody chain, whose finalized block trails its head by two
# blocks, A (the operator in effect's candidate), B and C register with their handover endpoints and
# delegate 50, 30 and 20 thousand MONKEY, and an election close adopts them as a rotation set. The
# other chain stands in for Ethereum: the node's own relayer relays each turn's RANDAO seed from a
# stand-in beacon node to the seed receiver. Eight days on the set takes effect, and five turns are
# drawn from relayed seeds, each at its draw deadline. At each boundary the incumbent hands the keys
# and the log to the turn's member by the existing handover, unless the member holds the turn on.
# It checks, at each boundary: that an admin command signed by the turn's key applies and the
# previous turn's key is refused; that the handover completes while the vault's finalized view
# still names the incumbent's release; that the member's first command is TurnTaken and the time in
# office follows it; that /v1/operator shows the rotation sets, today's and tomorrow's turns, and
# every assignment with its TurnTaken; and that the last member's log replays to its roots. Last,
# governance turns rotation off and, once the last turn ends, installs that turn's own candidate under
# another release, after which the finalized schedule names no turn.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib -- \
  --ignored --exact --nocapture \
  custody::tests::election_scenario::daily_turns::dev_scenario_each_daily_turn_takes_the_keys_at_its_boundary
