# Seed relay

Use this runbook to set up the seed relayer that every member of the rotation set runs, to deploy
the relay and receiver contracts, and to act when one of the seed relay's alerts fires. Each daily
operator turn is drawn from a random seed taken on Ethereum, and the relayer carries that seed to
Arbitrum, where the operator schedule reads it. Commands, configuration keys and their output call a
running trading server a node.

- [What a member needs](#what-a-member-needs)
- [Variables and checks](#variables-and-checks)
- [Deploy the relay and receiver](#deploy-the-relay-and-receiver)
- [When `seed_relay_failed` fires](#when-seed_relay_failed-fires)
- [When `seed_relayer_unfunded` fires](#when-seed_relayer_unfunded-fires)
- [When `seed_relayer_balance_unreadable` fires](#when-seed_relayer_balance_unreadable-fires)
- [When `seed_missing` fires](#when-seed_missing-fires)
- [How the seed relay works](#how-the-seed-relay-works)

The steps use the shell variables that [Variables](reference.md#variables) sets up, and the ones
below. [How the seed relay works](#how-the-seed-relay-works) explains the timing behind the alerts.

## What a member needs

Every member's trading server runs a relayer under `[seed_relay]`, whether it runs as the operator
or as a standby. The relayer runs for the whole life of the process: while the member follows the
operator between its turns, while it holds a turn, and across each handover. The relayer holds none
of the trading server's keys.

| Need | Detail |
| --- | --- |
| Three Ethereum execution providers | Each run by a different operator, listed in `seed_relay.providers`. The relayer acts only on a read that a strict majority of the providers answer identically. |
| A signing endpoint and a relayer key | `seed_relay.rpc_url` signs and sends transactions as `seed_relay.relayer`, which must not be the checkpoint relayer. Keep about a week of relays in the relayer's balance. A relay is one Ethereum transaction. Its gas is set by the relay's checks and by Arbitrum's inbox, and the trading server logs each relay's transaction hash, so `cast receipt` shows what it paid. Each relay also pays a ticket deposit of 150,000 times `l2_max_fee_per_gas`, and the unspent part of the deposit is refunded to the relayer on Arbitrum. |
| A beacon node | `seed_relay.beacon_api` must serve `/eth/v1/beacon/headers/finalized`, and `/eth/v2/debug/beacon/states/{slot}` in SSZ. Hosted providers often refuse the debug route, so run your own beacon node. A mainnet state is about 340 MB, and the relayer downloads one each turn. |
| Custody providers | The relayer reads the receiver on Arbitrum through `custody.evm.providers`. |

## Variables and checks

Use this procedure when you set up a member's relayer, and again before you rely on the relay.

1. Set the variables, with each value taken from the member's configuration:

   ```shell
   export L1_RPC=<ethereum provider>
   export SEED_RELAY=<seed_relay.relay>
   export SEED_RECEIVER=<seed_relay.receiver>
   export RELAYER=<seed_relay.relayer>
   export BEACON=<seed_relay.beacon_api>
   export SCHEDULE=<custody.evm.staking.registry.schedule>
   ```

   `<ethereum provider>` is the URL of one of `seed_relay.providers`. The other values are the
   configuration keys named. A URL that names a delivered key, such as `{beacon_key}`, needs that
   key filled in. `$RPC`, from [Variables](reference.md#variables), is an Arbitrum custody provider.

2. Check that the relay and the receiver name each other:

   ```shell
   cast call $SEED_RELAY 'receiver()(address)' --rpc-url $L1_RPC
   cast call $SEED_RECEIVER 'relay()(address)' --rpc-url $RPC
   ```

   The first command prints `$SEED_RECEIVER`, and the second prints `$SEED_RELAY`.

3. Check that both contracts were deployed with the same beacon genesis time:

   ```shell
   cast call $SEED_RELAY 'beaconGenesisTime()(uint64)' --rpc-url $L1_RPC
   cast call $SEED_RECEIVER 'beaconGenesisTime()(uint64)' --rpc-url $RPC
   ```

   Both commands print the same time, which is `1606824023` on mainnet.

4. Check that the relay accepts the current fork:

   ```shell
   cast call $SEED_RELAY 'acceptedFork(bytes4)(bool)' <fork version> --rpc-url $L1_RPC
   ```

   `<fork version>` is the `fork.current_version` of the beacon state, which is `0x06000000` for
   Fulu on mainnet. The command prints `true`.

5. Check that the beacon node serves the finalized header:

   ```shell
   curl -sf $BEACON/eth/v1/beacon/headers/finalized | jq .data.header.message.slot
   ```

   The command prints a slot number.

6. Read the relayer's metrics from the trading server's metrics listener:

   ```shell
   curl -s http://<metrics host>:9464/metrics | grep -E '^engine_seed_(latest_turn|relayer_balance_eth) '
   ```

   `<metrics host>` is the address of the member's metrics listener, which `metrics.listen` sets. A
   member running as a standby serves the same metrics. `engine_seed_latest_turn` reads three more
   than the turn in progress, because the turn whose draw deadline last passed and the two after it
   all have seeds. In the minutes after a draw deadline, before the next seed lands, it reads two
   more. `engine_seed_relayer_balance_eth` is above `seed_relay.min_balance`, which is in ether.

**How to tell it is resolved.** Every check above passes.

## Deploy the relay and receiver

Use this procedure once for each deployment, before the operator schedule is deployed. The relay on
Ethereum and the receiver on Arbitrum each name the other, so the receiver is deployed first, naming
the address the relay will have.

1. Find the address the relay will have, which is the deployer's next address on Ethereum:

   ```shell
   cast compute-address <deployer> --nonce $(cast nonce <deployer> --rpc-url $L1_RPC)
   ```

   `<deployer>` is the address that will deploy the relay. The command prints the address.

2. Deploy `SeedReceiver` on Arbitrum, naming the relay's address from step 1.

   **Changes the live system.** The owner must approve each deployment.

   Give the receiver the network's beacon genesis time, which is `1606824023` on mainnet, and a turn
   length of `86400` seconds, which is a day.

3. Deploy `SeedRelay` on Ethereum from the deployer, at the nonce used in step 1. Name Arbitrum's
   inbox and the receiver.

   **The deployer must send no other transaction on Ethereum before this deployment. Otherwise the
   relay lands at another address from the one the receiver names.**

   **Changes the live system.** The owner must approve each deployment.

   Give the relay the same beacon genesis time and turn length as the receiver. Give it the fork
   versions whose states it reads. Each version must be one whose `BeaconState` keeps the layout
   used from Electra onward, with its fields filling a tree of 64 leaves. On mainnet the versions
   are `0x05000000` for Electra and `0x06000000` for Fulu.

4. Deploy the operator schedule with this receiver. The schedule reads its seeds, the beacon genesis
   time and the turn length from the receiver.

   **Changes the live system.** The owner must approve each deployment.

**How to tell it is resolved.** The relay is at the address that the receiver names, and every check
in [Variables and checks](#variables-and-checks) passes.

A later Ethereum fork needs a release of the trading server whose relayer reads the new fork's
state. It also needs a new relay and receiver, unless the new fork's version was listed at
deployment because its specification keeps the layout.

A turn length shorter than a day is for development stacks only, and a production build of the
trading server refuses one.

## When `seed_relay_failed` fires

Use this procedure when `seed_relay_failed` fires. This member's relayer could not relay the seed of
a turn that the receiver lacks, or a seed that it relayed had not reached Arbitrum within
`seed_relay.resend_after`. Another member's relayer may still have relayed the seed. The alert
resolves once both upcoming turns have their seeds. Clients are not affected while any member's
relay succeeds.

1. Read the alert's message. It names the turn and the cause.

2. Match the message to one of the causes below, and follow its steps.

   - **The beacon API failed or refused, with a status code or a timeout.** Check the beacon node
     with the `curl` command in [Variables and checks](#variables-and-checks), and check that it
     serves states:

     ```shell
     curl -sfo /dev/null -w '%{http_code}' -H 'Accept: application/octet-stream' $BEACON/eth/v2/debug/beacon/states/<slot>
     ```

     `<slot>` is the slot that the finalized header returned. The command prints `200`.

   - **No majority of the Ethereum providers agree.** The message lists each provider's answer.
     Replace the failing provider. A provider is part of the configuration, so replacing it is a new
     release, as [Deploy and rollback](deploy-and-rollback.md) describes.

   - **The message says `reverted`.** A relay that this member sent reverted on Ethereum, and the
     relayer builds a new one at its next poll. If the revert repeats, the next attempt's gas
     estimate fails with the relay's reason, which the message names.

   - **The message says `this node reads Fulu states only` or `the relay refuses fork`, or names
     `ForkNotAccepted`.** Ethereum has forked. Relaying resumes only with a release of the trading
     server that reads the new fork's state. Unless the relay was deployed with the new fork's
     version, it also needs a new relay and a new receiver that names it. Both must have the
     schedule's turn length and beacon genesis time, which you read with:

     ```shell
     cast call $SCHEDULE 'turnLength()(uint64)' --rpc-url $RPC
     cast call $SCHEDULE 'beaconGenesisTime()(uint64)' --rpc-url $RPC
     ```

     The first command prints `86400`. The schedule refuses a receiver with another turn length. The
     release and the new contracts are the owner's to schedule. Until they are in place, every turn
     is drawn from the fallback seed, as [When `seed_missing` fires](#when-seed_missing-fires)
     describes.

   - **The message says a relay `has waited … for a block: replacing it at its nonce`.** The
     relayer's transaction stayed pending, usually because the base fee rose above its fee cap. The
     relayer has replaced it at the same nonce, with fees a quarter higher. It keeps one relay in
     flight at a time, so nothing queues behind it. Nothing needs to be done.

   - **The message says `a transaction this node is not tracking` holds the relayer's nonce.** A
     pending transaction holds the relayer's next nonce, and the relayer sends nothing until that
     transaction settles. The transaction was sent before a restart, or by a send whose answer was
     lost. Compare the pending and latest nonces:

     ```shell
     cast nonce $RELAYER --block pending --rpc-url $L1_RPC
     cast nonce $RELAYER --block latest --rpc-url $L1_RPC
     ```

     The pending nonce is higher while the transaction waits. If it does not mine, replace it with a
     transfer of zero value from the relayer to itself, at the latest nonce and with higher fees.

     **Changes the live system.** The replacement is a transaction on Ethereum, and the owner must
     approve it.

   - **The message says the relay `has not reached Arbitrum`.** The relay was sent to Ethereum, but
     no seed arrived within `seed_relay.resend_after`, and the relayer relays it again. Look up the
     relay transaction's retryable ticket on Arbitrum. If the ticket exists but its automatic
     redemption failed, anyone may redeem it with `ArbRetryableTx.redeem`. If the message never
     reached Arbitrum, the Arbitrum sequencer is withholding it. From 24 hours after the relay was
     included on Ethereum, anyone may force its inclusion through the sequencer inbox, as
     [Arbitrum's documentation](https://docs.arbitrum.io/how-arbitrum-works/sequencer) describes.
     The schedule leaves two days before the draw deadline, so a relay that is forced in still lands
     in time.

     **Changes the live system.** Redeeming the ticket and forcing the inclusion are transactions on
     Arbitrum and on Ethereum, and the owner must approve each one.

**How to tell it is resolved.** The alert resolves once both upcoming turns have their seeds, and
`engine_seed_latest_turn` reads three more than the turn in progress.

**When to escalate.** Tell the owner when Ethereum has forked, because the release and the new
contracts are the owner's to schedule.

## When `seed_relayer_unfunded` fires

Use this procedure when `seed_relayer_unfunded` fires. The relayer's Ethereum balance, read at a
block the providers agree is final, is below `seed_relay.min_balance`. The relayer stops relaying
once it cannot pay for a relay.

1. Send ETH to the relayer.

   **Changes the live system.** The transfer is a transaction on Ethereum, and the owner must
   approve it.

**How to tell it is resolved.** The alert resolves at the next balance read above the minimum.

## When `seed_relayer_balance_unreadable` fires

Use this procedure when `seed_relayer_balance_unreadable` fires. The relayer's balance, read through
the Ethereum providers at a block they agree is final, has been unreadable for ten minutes. While it
is unreadable, `engine_seed_relayer_balance_eth` stands still and `seed_relayer_unfunded` cannot
fire, although relays keep spending the relayer's balance.

1. Read the alert's message. It names the cause, which is either providers that fail
   `eth_getBalance` at their `finalized` block, or providers that disagree on that block.

2. Check the providers' health, and the `rpc_url` and `providers` keys in `[seed_relay]`.

3. Read the balance yourself:

   ```shell
   cast balance $RELAYER --block finalized --rpc-url $L1_RPC --ether
   ```

   The command prints the balance in ether.

4. If the balance is below `seed_relay.min_balance`, send ETH to the relayer.

   **Changes the live system.** The transfer is a transaction on Ethereum, and the owner must
   approve it.

**How to tell it is resolved.** The alert resolves at the next successful read.

## When `seed_missing` fires

Use this procedure when `seed_missing` fires. The turn whose draw deadline last passed has no
relayed seed. Every member's relayer failed, or every relay was withheld. The turn is therefore
drawn from the fallback: the last relayed seed, hashed with the turn's number and with the hash of
an Arbitrum block. The members' trading servers record that block's hash within seconds of the
deadline, after which the turn can be drawn as usual. The alert's message names the turn.

1. Check that the fallback hash is recorded:

   ```shell
   cast call $SEED_RECEIVER 'fallbackHash(uint64)(bytes32)' <turn> --rpc-url $RPC
   ```

   `<turn>` is the turn number from the alert's message. The command prints a nonzero hash once the
   members' trading servers have recorded it.

2. If the hash stays zero, call `fixFallback` twice at once, from two different blocks:

   ```shell
   cast send $SEED_RECEIVER 'fixFallback(uint64)' <turn> --rpc-url $RPC --account <sender>
   ```

   Run the command, wait for its block, and run it again. `<sender>` is a funded Foundry keystore
   account on Arbitrum. Step 1's command then prints a nonzero hash.

   **Changes the live system.** These are two transactions on Arbitrum, and they need the owner's
   approval like any contract call. Anyone may call `fixFallback`, and it moves no funds.

   **Act within about a minute. A block whose hash stays unrecorded for 256 blocks can never be
   recorded, and the receiver refuses with `FallbackExpired`. The turn is then never drawn, the
   member before it stays named through it, and each member's trading server raises
   `seed_fallback_lapsed` until the turn's boundary.**

3. Find why no seed was relayed. Check `seed_relay_failed` on each member's trading server, and
   follow [When `seed_relay_failed` fires](#when-seed_relay_failed-fires).

**How to tell it is resolved.** The alert resolves once a turn reaches its draw deadline with its
relayed seed. The next turn's draw deadline is a day later.

**When to escalate.** Tell the owner if `seed_fallback_lapsed` fires, because a turn will not be
drawn.

## How the seed relay works

Each daily operator turn is seeded by the RANDAO mix at the end of one Ethereum beacon epoch. The
schedule fixes that epoch from the turn's number:

- Turn `n` starts at its boundary, 22:30 UTC on day `n` of the Unix epoch.
- The turn's draw deadline is a day earlier than its boundary.
- The turn's seed is the mix of the epoch that holds the time two days before the draw deadline.

The relay and the receiver are deployed with a turn length of a day, 86400 seconds. A development
stack may use a shorter turn, with the draw deadline one turn before the boundary and the epoch two
turns before the deadline.

`SeedRelay` on Ethereum proves the mix from an EIP-4788 beacon block root, and sends it to
`SeedReceiver` on Arbitrum through Arbitrum's inbox. From the draw deadline, the receiver's answer
for the turn is final. The answer is the relayed mix, if one arrived. If none arrived, the answer is
the last relayed seed, hashed with the turn's number and with the hash of an Arbitrum block. That
block is the one in which the turn's first `fixFallback` or `draw` call after the deadline landed,
and its hash counts once a call from a later block has recorded it.

Anyone may relay a seed, and every relay of a turn carries the same seed. The relay therefore needs
only one working relayer among the members of the rotation set.
