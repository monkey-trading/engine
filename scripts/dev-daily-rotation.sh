#!/usr/bin/env bash
# Runs the daily rotation scenario on two isolated Anvil chains, with time warped by the chains and
# the node's test clock together. On the custody chain, whose finalized block trails its head by two
# blocks, A (the operator in effect's candidate), B and C register with their handover endpoints and
# API hostnames and delegate 50, 30 and 20 thousand MONKEY, and an election close adopts them as a
# rotation set. B and C follow A's signed stream from boot into live replicas, as each member follows
# the operator in effect between its turns, while a load generator places and cancels BTC-USD orders
# on the node holding the turn. The other chain stands in for Ethereum: the node's own relayer relays
# each turn's RANDAO seed from a stand-in beacon node to the seed receiver. Eight days on the set takes
# effect, and five turns are drawn from relayed seeds, each at its draw deadline. At each boundary the
# turn's member takes the keys and only the tail past its replica's tip from the incumbent, unless it
# holds the turn on, and starts from the replica; the incumbent then follows it.
# It checks, at each boundary: that an admin command signed by the turn's key applies and the
# previous turn's key is refused; that the handover completes while the vault's finalized view
# still names the incumbent's release; that only the tail's bytes move and no whole-log replay runs
# in the switch, whose window, the wall time from the incumbent's last command to the member's
# TurnTaken, it prints and holds to a budget; that the member's first command is TurnTaken and the
# time in office follows it; and that /v1/operator shows the rotation sets, today's and tomorrow's
# turns, and every assignment with its TurnTaken. The holders post checkpoints. A withdrawal
# approved during A's turn waits for the next boundary's log check, though the vault holds its
# checkpoint, and pays through the vault only once B's handover has verified A's log; a second,
# approved during B's turn, waits for the boundary B holds on through, where a verification from B's
# own release is refused and, with A first in the turn's order and C stopped, the check stays due
# until C follows again and verifies it, releasing the withdrawal with no handover. A stream client
# follows each holder at its hostname, resuming from its seq against the same enclave address, and
# the last member's log replays to its roots. Last, governance turns rotation off and, once the last turn ends, installs that turn's
# own candidate under another release, after which the finalized schedule names no turn. It runs no
# ACME; tls::acme::tests::sealed checks that a member's next turn reuses its certificate.
# Needs Anvil, Forge, Python 3, Cargo, and installed Soldeer dependencies. No external RPC or keys are used.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root/contracts" && forge build)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib -- \
  --ignored --exact --nocapture \
  custody::tests::election_scenario::daily_turns::dev_scenario_each_daily_turn_takes_the_keys_at_its_boundary
