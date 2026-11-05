#!/usr/bin/env bash
# Regenerates contracts/test/fixtures/seed-relay/mainnet.json, the recorded mainnet data the
# SeedRelay tests prove against, from a named mainnet epoch:
#   scripts/seed-relay-fixtures.sh <epoch> [missed-epoch]
# The epoch must be one SeedSchedule fixes for some turn under mainnet's genesis time, and within
# the 65,536 epochs before the finalized state. The script downloads that state (about 340 MB, SSZ)
# from a checkpoint-sync server, builds the witnesses with the seed relayer's own code
# (crates/node/examples/seed_witness.rs), and checks every mix two ways that do not use it: the
# beacon API's randao read, and the prevRandao of the first execution block after the epoch. It also
# records an epoch whose next epoch's first slot was missed (the latest before the state unless
# named): the last block before that slot, whose EIP-4788 entry is the first block after the gap.
# Overrides: BEACON_API (headers, blocks, randao), STATE_API (the finalized state), EXECUTION_RPC.
# Needs curl, jq, python3 and Cargo. Reads public endpoints only; writes only the fixture.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
epoch="${1:?usage: seed-relay-fixtures.sh <epoch> [missed-epoch]}"
missed="${2:-}"
beacon="${BEACON_API:-https://ethereum-beacon-api.publicnode.com}"
state_api="${STATE_API:-https://sync-mainnet.beaconcha.in}"
rpc="${EXECUTION_RPC:-https://ethereum-rpc.publicnode.com}"
genesis=1606824023
out="$root/contracts/test/fixtures/seed-relay/mainnet.json"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

get() { curl -sSf --max-time 60 --retry 3 "$@"; }
rpc_block() {
  get -X POST -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBlockByNumber\",\"params\":[\"$1\",false]}" "$rpc" |
    jq -e '.result'
}
# The execution block in the beacon block at `slot`, which must exist.
execution_at_slot() {
  local number
  number="$(get "$beacon/eth/v1/beacon/blinded_blocks/$1" | jq -er '.data.message.body.execution_payload_header.block_number')"
  rpc_block "$(printf '0x%x' "$number")"
}
# The first existing slot at or after `$1`, within an epoch of it.
first_block_from() {
  local slot="$1"
  until get -o /dev/null "$beacon/eth/v1/beacon/headers/$slot" 2>/dev/null; do
    slot=$((slot + 1))
    [ "$slot" -lt $(($1 + 32)) ] || { echo "no block in the 32 slots from $1" >&2; exit 1; }
  done
  echo "$slot"
}
# The mix at the end of `$1`, read without the relayer's code: the beacon API's, and the first
# execution block after it's prevRandao (`mixHash`).
check_mix() {
  local epoch="$1" seed="$2" api first execution
  # Any state past the epoch, and within 65,536 epochs of it, holds the same final mix.
  api="$(get "$beacon/eth/v1/beacon/states/head/randao?epoch=$epoch" | jq -er '.data.randao')"
  first="$(first_block_from $(((epoch + 1) * 32)))"
  execution="$(execution_at_slot "$first")"
  [ "$api" = "$seed" ] && [ "$(jq -er '.mixHash' <<< "$execution")" = "$seed" ] || {
    echo "epoch $epoch: the witness proves $seed; the beacon API reads $api and slot $first's block $execution" >&2
    exit 1
  }
  jq -c --argjson slot "$first" --arg randao "$api" \
    '{slot: $slot, blockNumber: .number, prevRandao: .mixHash, beaconApiRandao: $randao}' <<< "$execution"
}

scheduled="$(python3 -I -c "
import sys
e, g = int(sys.argv[1]), int(sys.argv[2])
turns = [t for t in range(18000, 40000) if (t * 86400 + 81000 - 86400 - 172800 - g) // 384 == e]
print(turns[0] if turns else '')" "$epoch" "$genesis")"
[ -n "$scheduled" ] || { echo "epoch $epoch seeds no turn under mainnet's genesis time" >&2; exit 1; }

# The finalized state, and its block's header and root.
curl -sSf --max-time 1200 -H 'Accept: application/octet-stream' -D "$work/state.headers" -o "$work/state.ssz" \
  "$state_api/eth/v2/debug/beacon/states/finalized"
grep -qi '^eth-consensus-version: fulu' "$work/state.headers" || { echo "the state is not a Fulu state" >&2; exit 1; }
state_slot="$(python3 -I -c "import sys,struct; print(struct.unpack('<Q', open(sys.argv[1],'rb').read(48)[40:48])[0])" "$work/state.ssz")"
get "$beacon/eth/v1/beacon/headers/$state_slot" > "$work/header.json"
beacon_root="$(jq -er '.data.root' "$work/header.json")"
witness() {
  CARGO_INCREMENTAL=0 cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p engine-node \
    --example seed_witness -- "$work/state.ssz" "$work/header.json" "$1"
}
witness "$epoch" > "$work/witness.json"
first="$(check_mix "$epoch" "$(jq -er '.seed' "$work/witness.json")")"

# The EIP-4788 entry that holds the state's block root: the next block's execution payload.
child_slot="$(first_block_from $((state_slot + 1)))"
child="$(execution_at_slot "$child_slot")"
[ "$(jq -er '.parentBeaconBlockRoot' <<< "$child")" = "$beacon_root" ] ||
  { echo "slot $child_slot's execution block does not name $beacon_root" >&2; exit 1; }
timestamp="$(($(jq -er '.timestamp' <<< "$child")))"

# An epoch whose next epoch starts with a missed slot.
if [ -z "$missed" ]; then
  missed=$((state_slot / 32 - 2))
  until ! get -o /dev/null "$beacon/eth/v1/beacon/headers/$(((missed + 1) * 32))" 2>/dev/null; do
    missed=$((missed - 1))
    [ "$missed" -gt $((state_slot / 32 - 5000)) ] || { echo "no missed first slot in 5000 epochs" >&2; exit 1; }
  done
fi
get -o /dev/null "$beacon/eth/v1/beacon/headers/$(((missed + 1) * 32))" 2>/dev/null &&
  { echo "slot $(((missed + 1) * 32)) is not missed" >&2; exit 1; }
last_slot=$(((missed + 1) * 32 - 1))
until get -o "$work/last.json" "$beacon/eth/v1/beacon/headers/$last_slot" 2>/dev/null; do
  last_slot=$((last_slot - 1))
  [ "$last_slot" -gt $((missed * 32)) ] || { echo "epoch $missed has no block" >&2; exit 1; }
done
witness "$missed" > "$work/missed.json"
after="$(check_mix "$missed" "$(jq -er '.seed' "$work/missed.json")")"
after_block="$(execution_at_slot "$(jq -er '.slot' <<< "$after")")"
[ "$(jq -er '.parentBeaconBlockRoot' <<< "$after_block")" = "$(jq -er '.data.root' "$work/last.json")" ] ||
  { echo "the block after epoch $missed does not follow slot $last_slot" >&2; exit 1; }

mkdir -p "$(dirname "$out")"
jq -n --slurpfile witness "$work/witness.json" --slurpfile missed "$work/missed.json" \
  --slurpfile last "$work/last.json" --argjson turn "$scheduled" --argjson timestamp "$timestamp" \
  --argjson genesis "$genesis" --argjson first "$first" --argjson after "$after" \
  --argjson afterTimestamp "$(($(jq -er '.timestamp' <<< "$after_block")))" '{
    beaconGenesisTime: $genesis,
    turn: $turn,
    timestamp: $timestamp,
    proof: $witness[0],
    firstBlockAfter: $first,
    missed: {
      proof: $missed[0],
      firstBlockAfter: $after,
      lastBlockBefore: {
        timestamp: $afterTimestamp,
        beaconRoot: $last[0].data.root,
        header: ($last[0].data.header.message | {
          slot: (.slot | tonumber), proposerIndex: (.proposer_index | tonumber),
          parentRoot: .parent_root, stateRoot: .state_root, bodyRoot: .body_root
        })
      }
    }
  }' > "$out"
echo "wrote $out: epoch $epoch (turn $scheduled) and missed-slot epoch $missed from slot $state_slot's state"
