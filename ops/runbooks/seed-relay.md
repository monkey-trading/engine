# Seed relay

Each daily operator turn is seeded by the RANDAO mix at the end of one Ethereum beacon epoch. The
schedule fixes that epoch from the turn number: turn `n` starts at its boundary, 22:30 UTC on day `n`
of the Unix epoch; its draw deadline is a day earlier; and its seed is the mix of the epoch that holds
the time two days before the draw deadline. `SeedRelay` on Ethereum proves the mix from an EIP-4788
beacon block root and sends it to `SeedReceiver` on Arbitrum through Arbitrum's inbox. From the draw
deadline the receiver's answer for the turn is final: the relayed mix, or, if none arrived, the last
relayed seed hashed with the turn number.

Anyone may relay, and every relay of a turn carries the same seed, so the relay needs one working
relayer among the rotation set. Every member's node runs one under `[seed_relay]`, in either mode,
operator or standby. It holds no key of the node's. Read [reference.md](reference.md) first.

## What a member needs

| Need | Detail |
| --- | --- |
| Three Ethereum execution providers | Run by different operators (`seed_relay.providers`). Every read the relayer acts on is one a strict majority of them gives alike. |
| A signing endpoint and a relayer key | `seed_relay.rpc_url` signs and sends as `seed_relay.relayer`, which must not be the checkpoint relayer. Keep about a week of relays in it. A relay is one Ethereum transaction, whose gas the relay's checks and Arbitrum's inbox set (the node logs each one's hash, and `cast receipt` shows what it paid), plus a ticket deposit of 150,000 times `l2_max_fee_per_gas`, whose unspent part is refunded to the relayer on Arbitrum. |
| A beacon node | `seed_relay.beacon_api` must serve `/eth/v1/beacon/headers/finalized` and `/eth/v2/debug/beacon/states/{slot}` in SSZ. Hosted providers often refuse the debug route; run your own. A mainnet state is about 340 MB, downloaded once a turn. |
| Custody providers | The relayer reads the receiver through `custody.evm.providers`. |

## Variables and checks

```sh
export L1_RPC=https://...               # one of seed_relay.providers
export SEED_RELAY=0x...                 # seed_relay.relay, on Ethereum
export SEED_RECEIVER=0x...              # seed_relay.receiver, on Arbitrum
export RELAYER=0x...                    # seed_relay.relayer
export BEACON=https://...               # seed_relay.beacon_api
```

Check, before relying on the relay:
- `cast call $SEED_RELAY 'receiver()(address)' --rpc-url $L1_RPC` prints `$SEED_RECEIVER`, and
  `cast call $SEED_RECEIVER 'relay()(address)' --rpc-url $RPC` prints `$SEED_RELAY`.
- `cast call $SEED_RELAY 'beaconGenesisTime()(uint64)' --rpc-url $L1_RPC` and the same call to the
  receiver on `$RPC` print the same time, `1606824023` on mainnet.
- `cast call $SEED_RELAY 'acceptedFork(bytes4)(bool)' <the current fork version> --rpc-url $L1_RPC`
  prints `true`; the version is the state's `fork.current_version`, `0x06000000` for Fulu on mainnet.
- `curl -sf $BEACON/eth/v1/beacon/headers/finalized | jq .data.header.message.slot` prints a slot.
- The node's `engine_seed_latest_turn` metric is the current turn or one past it, and
  `engine_seed_relayer_balance_eth` is above `seed_relay.min_balance`.

## When `seed_relay_failed` fires

This node's relayer could not relay a turn the receiver lacks. Another member's may still have, and
the alert ends once both upcoming turns have their seeds. The message names the turn and the cause:

- **The beacon API failed or refused** (a status, a timeout, or "serves a … state"): check the node
  with the `curl` above and `curl -sfo /dev/null -w '%{http_code}' -H 'Accept: application/octet-stream'
  $BEACON/eth/v2/debug/beacon/states/<slot>`.
- **No majority of Ethereum providers agree**: the message lists each provider's answer. Replace a
  failing provider through a config change, which is a new release ([deploy-and-rollback.md](deploy-and-rollback.md)).
- **A relay reverted, or its estimate names `ForkNotAccepted`**: a fork has changed the beacon
  state, and the relay refuses its states until a relay that knows the new layout is deployed with a
  receiver that names it. That is a new deployment; take it to the owner. Until then turns draw from
  the fallback.
- **"has not reached Arbitrum"**: the relay was sent to Ethereum but no seed arrived within
  `resend_after`, and the relayer relays it again. Look up the relay transaction's retryable ticket on
  Arbitrum. If the ticket exists but its automatic redemption failed, anyone may redeem it with
  `ArbRetryableTx.redeem`. If the message never reached Arbitrum, the sequencer is withholding it:
  24 hours after the relay was included on Ethereum, anyone may force-include it through the
  sequencer inbox ([Arbitrum's documentation](https://docs.arbitrum.io/how-arbitrum-works/sequencer)).
  The schedule leaves two days, so a force-included relay still lands before the draw deadline.
  **Changes the live system** (a transaction on Ethereum or Arbitrum): get the owner's approval.

## When `seed_relayer_unfunded` fires

The relayer's Ethereum balance, read at a block the providers agree is final, is below
`seed_relay.min_balance`. Send it ETH. **Changes the live system** (a transfer): get the owner's
approval. The alert ends at the next read above the minimum.

## When `seed_missing` fires

The turn whose draw deadline last passed has no relayed seed, so it draws from the last relayed seed
hashed with its number. That outcome is fixed: nothing can change the turn now. Every member's
relayer failed, or every relay was withheld, so look at `seed_relay_failed` on each member's node and
follow the section above. The next turn's draw deadline is a day later. The alert ends once a turn
reaches its draw deadline with its seed.
