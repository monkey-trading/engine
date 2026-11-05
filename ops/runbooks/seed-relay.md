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
- The node's `engine_seed_latest_turn` metric is three past the turn in progress (the turn whose
  draw deadline last passed and the two after it all have seeds), or two past it in the minutes after a
  draw deadline before the next seed lands, and `engine_seed_relayer_balance_eth` is above
  `seed_relay.min_balance`. A node running as a standby serves no metrics: read its relayer's alerts.

## When `seed_relay_failed` fires

This node's relayer could not relay a turn the receiver lacks. Another member's may still have, and
the alert ends once both upcoming turns have their seeds. The message names the turn and the cause:

- **The beacon API failed or refused** (a status or a timeout): check the node
  with the `curl` above and `curl -sfo /dev/null -w '%{http_code}' -H 'Accept: application/octet-stream'
  $BEACON/eth/v2/debug/beacon/states/<slot>`.
- **No majority of Ethereum providers agree**: the message lists each provider's answer. Replace a
  failing provider through a config change, which is a new release ([deploy-and-rollback.md](deploy-and-rollback.md)).
- **"reverted"**: a relay this node sent reverted on Ethereum, and the relayer builds a new one at
  the next poll. If it repeats, the next attempt's gas estimate fails with the relay's reason, which
  the message names.
- **"this node reads Fulu states only", "the relay refuses fork", or `ForkNotAccepted`**: Ethereum
  has forked. Relaying resumes only with a node release that reads the new fork's state and, unless
  the relay was deployed with the new fork's version, a new relay and a receiver that names it. Both
  are the owner's to schedule; until then every turn draws from the fallback.
- **"waited … for a block: replacing it at its nonce"**: the relayer's transaction stayed pending,
  usually because the base fee rose past its fee cap, and the relayer replaced it at the same nonce
  with fees a quarter higher. It keeps one relay in flight at a time, so nothing queues behind it.
- **"a transaction this node is not tracking"**: a pending transaction holds the relayer's next nonce,
  one sent before a restart or by a send whose answer was lost, and the relayer sends nothing until it
  settles. Compare `cast nonce $RELAYER --block pending --rpc-url $L1_RPC` with `--block latest`. If it
  does not mine, replace it with a zero-value transfer from the relayer to itself at the latest nonce
  and higher fees. **Changes the live system** (a transaction on Ethereum): get the owner's approval.
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

## When `seed_relayer_balance_unreadable` fires

The relayer's balance, read through the Ethereum providers at a block they agree is final, has failed
for ten minutes, so the funding alert cannot fire. The message names the cause: providers that fail
`eth_getBalance` at their `finalized` block, or that disagree on that block. Check the providers'
health and the `rpc_url` and `providers` in `[seed_relay]`. Read the balance yourself with
`cast balance <relayer> --block finalized --rpc-url $RPC` and send ETH if it is below
`seed_relay.min_balance`. The alert ends at the next successful read.

## When `seed_missing` fires

The turn whose draw deadline last passed has no relayed seed, so it draws from the last relayed seed
hashed with its number. That outcome is fixed: nothing can change the turn now. Every member's
relayer failed, or every relay was withheld, so look at `seed_relay_failed` on each member's node and
follow the section above. The next turn's draw deadline is a day later. The alert ends once a turn
reaches its draw deadline with its seed.
