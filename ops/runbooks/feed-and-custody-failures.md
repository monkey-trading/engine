# Feed and custody failures

The node depends on four kinds of outside service: price feeds, custody RPC providers, the checkpoint
endpoint, and trusted time. Each fails closed: a market with no price fills nothing, a custody
quorum that disagrees credits nothing, a clock that cannot be trusted stamps nothing. None of them
loses a recorded command. Read [reference.md](reference.md) first.

**Every config change below is a new release.** The venues, symbols, providers, endpoints and relayer
are in the config file, whose hash is part of the release's identity. There is no way to change them
on a running node: [deploy-and-rollback.md](deploy-and-rollback.md). Plan a replacement as an
upgrade, not as a hot fix.

## Price feeds

### How a price exists

Each market needs at least `min_sources` quotes (2 for every launch market) no older than its
`max_age_ms` (1 second), agreeing within `max_divergence_bps`. Otherwise the market has no price:
no order fills, **closes included**, and positions in it that fall below the close-out level wait
until it prices again. A market priced from two sources has no price when either fails or goes quiet
for longer than `max_age_ms`. That holds for the crypto markets and for the vendor-priced ones.

### A crypto venue is down or quiet

BTC-USD and ETH-USD are priced from Coinbase and Kraken only, with `min_sources = 2` and
`max_age_ms = 1000`. If either venue disconnects, both markets have no price: no fills and no
close-outs until it is back. This halts those markets. A book that stands is kept quoting by the
connection's pings for about five seconds past its last change, and for about a second once the
venue stops answering them, so a symbol whose book shows no change for more than about six seconds
halts its own market.

**Why it is the case to watch.** At the owner's decided terms (1% margin, 100x, up to $1M, close-out
at 50% of margin) a full 100x position closes out about 0.5% from its entry and loses more than its
balance past about 1%. The loss beyond a balance falls on the house (`bad_debt`). During an unpriced spell
in a fast move nothing can close out, and when the price returns the gap is taken at once. Read the
live terms from `margin_tiers` in `curl -sf $API/v1/markets`.

**See it.** `venue_feed_down` or `venue_symbol_silent` (Warning) names the venue; `market_no_price`
(Critical) follows after `alerts.no_price_after`; `engine_market_priced{market="1"}` and
`{market="2"}` read `0`; `/v1/markets` shows no `mark` for them; fills stop.

**You can:**
1. Make BTC-USD and ETH-USD reduce-only at once ([reduce-only.md](reduce-only.md)). It stops new
   risk from the first fill after the price returns. It does not close anything and cannot while
   there is no price.
2. Find out which venue and why: its status page, your account or IP limits, DNS and egress from the
   host. The node reconnects by itself, backing off from `reconnect_min` to `reconnect_max` (30
   seconds in the template).
3. Tell the owner, with the time the market lost its price, the venue, and `house` NAV from
   `/v1/house`.

**You cannot:**
- Set or override a price. No command does.
- Lower `min_sources`, or `max_age_ms`, or add a third source during the outage. The venues are in
  the config (a new release, [deploy-and-rollback.md](deploy-and-rollback.md)) and the price rules are
  market parameters the owner changes by an admin command; neither is a hot fix.
- Close positions or force a close-out without a price. Raising the stress floor does not help
  either: it widens spreads on prices that do not exist.

The books are safe throughout; what is at stake is the house's exposure at the gap. Keep the time the
price returned, and `bad_debt` pages after it, for the owner.

### Tightening `max_age_ms`

A change to a market's `max_age_ms` is checked, when it is sequenced, against the quotes the market
holds. While the market, or a market quoted in the currency it converts, has open positions, it
applies only if those quotes, at their age then, still price it under the shorter limit; otherwise
it is refused with "price rules it cannot price under" (or, for a held market that converts at the
one being changed, "which the price rules cannot price under"). A venue's quote keeps the venue's
own timestamp, and the feed's repeat rule only delays a repeat of an unchanged price, so a quiet mid
leaves quotes older than a tighter limit even though the venues are healthy.

1. Check that the market is open and priced: `engine_market_priced` reads 1 for it, as under
   [Find what is down](#find-what-is-down). A closed market holds old quotes and refuses every
   tightening until it opens. `market_no_price` only fires after `alerts.no_price_after`, so its
   silence proves nothing.
2. Sign the change with `upsert-market` as under [Futures markets](#futures-markets). **Changes the
   live system.** Needs the owner's approval of the exact spec. If it is refused, wait for the venues
   to quote again, which moves the mid or fires the repeat, and sign the same change once more.
3. For a large cut, tighten in steps, each signed after the venues have quoted again, so each step
   still prices the market from quotes that have arrived since the last. **Changes the live system.**
   Each step is its own `upsert-market`, so each needs the owner's approval of its exact spec, or
   one approval naming the whole sequence of limits.
4. Failing that, make every held market the refusal names reduce-only first, the edited market and
   any market that converts at it ([reduce-only.md](reduce-only.md)), then sign the change again: the
   check then does not apply. **Changes the live system.** Needs the owner's approval of the
   reduce-only commands, the change, and the commands below. Then check, as in step 1, that
   `engine_market_priced` reads 1 for the edited market and each market that converts at it. The
   `mark` in `/v1/markets` is the last stored value and stays after pricing is lost, so it proves
   nothing. If one reads 0, restore the previous limit, which is always accepted, and wait for the
   venues to quote again: loosening discards quotes stale under the tighter limit. Either way, resume
   normal trading as in [reduce-only.md](reduce-only.md#resume-normal-trading).

### Find what is down

```sh
curl -sf $API/v1/markets | jq -r '.[] | select(.open and (.mark == null)) | "\(.id) \(.symbol) no mark"'
curl -s http://<metrics host>:9464/metrics | grep -E '^engine_market_priced'     # 0 = no usable index
```

Open markets with no mark are the `market_no_price` case. Match the alerts to the cause:

| Alert | Level | Meaning | Do |
| --- | --- | --- | --- |
| `venue_feed_down` | Warning | A connection to a venue or vendor is down; the node reconnects with backoff (`reconnect_min` to `reconnect_max`). Fields: `venue`, `feed`. | Check the vendor's status and the account (plan, key, entitlement, connection limit). Nothing to do on the node. |
| `venue_symbol_silent` | Warning | A symbol has had no fresh quote for `feeds.venues.timeout` while its market is open. The connection is replaced. | Check the symbol is still the vendor's (a ticker or contract change); a wrong symbol is a config fix. |
| `venue_message_refused` | Warning | The node refused a vendor message (malformed, unknown). At most one a minute, with a count; API keys are redacted. | Read the excerpt. A vendor format change needs a node release. |
| `venue_clock_ahead` | Warning | A venue's timestamps are well ahead of the host clock. | Check `clock_drift` and the host clock; quotes ahead of sequenced time are rejected (`prices_rejected`, reason `quote_ahead`). |
| `venue_delayed` | Warning | A connection's pings show its stream may be further behind than `feeds.venues.max_delay`, so what it reads counts as no quote, and a market needing that venue has no price. It resolves when a frame next counts. Fields: `venue`, `feed`. | Check the network path to the venue, and the host's load. If the path is healthy, the host may be holding the stream back: read [incident-response.md](incident-response.md). |
| `prices_rejected` | Warning | The engine refused a price update. The `reason` is one of `unknown_market`, `malformed_quote`, `quote_ahead`, `market_closed`, `contract_not_taken`, `other`. | `contract_not_taken` on a futures market means a quote for a contract the market is not on: see below. |
| `market_no_price` | Critical | An open market a feed prices has had no usable price for `alerts.no_price_after`. | Below. |
| `market_unfed` | Warning | No configured feed prices this market, so it cannot trade. It never pages `market_no_price`. | Expected only for markets you have not wired; otherwise a config gap. |
| `sequencer_stalled` | Critical | The sequencer answered no sample for `alerts.no_price_after`. | [restart-and-recovery.md](restart-and-recovery.md). |

### A market has no price (`market_no_price`)

1. Find which sources are missing from the alert text ("only N fresh sources, need 2"), and match to
   `venue_feed_down` or `venue_symbol_silent` for that market.
2. If the market is closed on its calendar, it is not a fault: the alert covers open markets only.
3. If one vendor is down: wait for its reconnect, check its status page, and check your account. If
   the vendor will be down for long, the owner can decide to ship a release that prices the market
   from another source (a config change) or to leave the market without a price. The books are safe
   either way: a position in a market with no price waits.
4. If the sources disagree (both quote, divergence over the bound): the engine refuses the price on
   purpose. Look at the two quotes; do not widen `max_divergence_bps` to make it go away.
5. While a market is unpriced, consider [reduce-only.md](reduce-only.md) for it, so that nothing new
   is opened the moment it prices again. That is the owner's call, not a requirement.

### Futures markets

An index or commodity market is priced from its front futures contract and rolls onto the next at the
time its listing names.

- `futures_roll_overdue` (Warning): the roll time passed with no gap recorded between the contracts.
  Check the feeds quoting the **next** contract: the roll records the gap between the two contracts' prices, so it needs quotes for the next one.
- `futures_last_contract` (Warning): the market prices from the last contract its listing names.
  The owner lists the next contract before it expires, by an `upsert-market` admin command carrying
  the market's whole spec with the new `contracts`. **Changes the live system.** Needs the owner's
  approval of the exact spec. A file holds the keys of a `[[genesis.markets]]` entry without the
  table header:

  ```sh
  $ENGINE admin --config $CONFIG --node $API --pin $PIN upsert-market market.toml
  ```

  The vendors' symbols for the new contract must be in the config first; that is a new release.
- `contract_not_taken` in `prices_rejected`: a quote for a contract the market is not taking now.
  Expected around a roll; persistent means the feed's symbols and the listing's contracts disagree.

### Benchmark rates

Financing uses dated rates from a separate worker ([deploy-and-rollback.md](deploy-and-rollback.md),
section 5). It does not affect prices.

- `benchmark_fetch_failed`, `benchmark_sync_failed`, `benchmark_schedule_watch_failed` (Warnings): a
  publisher fetch or a call to the node failed; the worker retries by itself. Check the
  publisher's endpoint and the worker host.
- `benchmark_rate_refused` (Warning): from `V46`, a published rate is more than 300 bps a day from the
  applied rates next to its date, or differs from a date already applied. The worker leaves it out and
  signs the rest, so nothing else waits on it, and the last applied rate keeps financing. The limit
  grows with each day, so a real move lands once it fits; a revision of an applied date never does.
  Check the rate against the publisher. Signing it by hand is refused for the same reason.
- `benchmark_rate_stale` (Critical): the newest observation is older than five weekdays at a
  rollover; financing still applies. `benchmark_rate_missing` (Critical): no observation, no
  financing is charged on that benchmark's markets. `benchmark_rate_undated` (Critical): a legacy
  rate with no date still applies. Fix the worker and its publisher fetch first. As a last resort
  the owner may set a rate by hand, which is an admin command and a financing decision for every
  holder: `$ENGINE admin … set-benchmark-rate <NAME> <bps a year>`.
- `benchmark_sync_pending` (Warning): the node is waiting for the worker's signed history before it
  catches up. Start the worker.

### Trusted time

Every stamp is checked against signed Roughtime readings from independent servers. Nothing is
sequenced before the first sample, and sequencing halts when the host clock is outside
`time.max_drift` of them or no sample has arrived for 60 seconds.

| Alert | Meaning | Do |
| --- | --- | --- |
| `roughtime_server_failing` (Warning) | One server's queries keep failing. | Check the network path (UDP) to it. A majority of the servers must agree, so one failing is slack used up. |
| `roughtime_server_disagrees` (Warning) | One server signed a time outside the agreed sample. | Do not act on one server; if it persists, the owner may drop it (a new release). |
| `trusted_time_no_sample` (Warning) | No sample could be taken. | UDP egress to the servers; the host's resolver. |
| `trusted_time_unavailable` (Critical) | No sample within the grace: sequencing has halted. | As above. Sequencing resumes by itself when a sample arrives. |
| `host_clock_behind` (Warning) | The host clock has fallen a second behind monotonic time. Stamps keep monotonic time's pace, so quotes still age out. A clock that ran ahead and came back leaves stamps ahead of it, and markets may have no price until that gap drains, at 1% of elapsed time. | Fix the host's clock. The host controls the clock, so also read [incident-response.md](incident-response.md). |
| `clock_drift` (Critical) | The host clock or a stamp is outside the drift bound of trusted time: sequencing halted. | Fix the host's clock. The host controls the clock, so also read [incident-response.md](incident-response.md) if it was a large jump. |

## Custody RPC providers

The custody watcher reads the vault through at least three independently operated providers
(`custody.evm.providers`) and accepts an answer only when a strict majority give the identical
answer: two of three, three of four. It credits deposits and unlocks principal from those answers,
so a quorum loss stops crediting. Trading continues.

| Alert | Level | Meaning | Do |
| --- | --- | --- | --- |
| `custody_rpc_provider_failed` | Warning | One provider failed an operation (field `provider`, `operation`). | The quorum holds. Check that provider; with three providers this is your last slack. |
| `custody_rpc_provider_disagreed` | Warning | One provider's answer differs from the quorum's. | A lagging or forked provider, or a wrong one. Compare it by hand (below). Do not ignore it for long: it is also what an attacker's provider looks like. |
| `custody_rpc_quorum_lost` | Critical | No answer has a majority: crediting is paused. With `quorum=operators` it is a Warning and pauses only the operator list. | Below; for the operator list, see `operators_read_failed`. |
| `operators_read_failed` | Warning | The operator list (`GET /v1/operators`) cannot be read at a final block: no majority for a lens call (field `operation` names which: `lens_registry`, `lens_overview`, `lens_candidates_<page>`), the wrong chain or token, or an inconsistent lens. The route serves the last list until `finality_stalled_after`, then answers 503. Nothing else is paused. | Check the providers as above, and that `custody.evm.staking.registry.lens` is the `RegistryLens` beside the configured registry; a provider that caps the gas of an `eth_call` fails the larger pages. |
| `custody_rpc_failed`, `custody_chain_unsettled` | Warning | A poll failed on a read, or the chain's answers are lagging. The watcher retries every poll interval. | Look at the providers. If it lasts `stall_critical_after`, `custody_deposits_stalled` follows. |
| `custody_chain_inconsistent` | Critical | The chain's answers contradict the config or the cursor (a different chain, vault or token, or a final block that moved back). | Do not change anything. This is not an outage; call the owner. |
| `custody_deposits_stalled` | Critical | No vault deposit was read for `custody.evm.stall_critical_after`. | The rows above; deposits are unscanned. A checkpoint deposit left uncredited this long also pauses withdrawals ([withdrawal-stalls.md](withdrawal-stalls.md)). |
| `custody_deposits_lagging` | Critical | Deposit reads trail the final block for that long. | Slow or rate-limited providers; the range per request (`max_block_range`) or the poll interval is wrong for them. |
| `custody_head_stale` | Critical | An endpoint's head is not advancing: its newest block is old. | The provider, or the chain itself, has stopped. |
| `custody_finality_stalled` | Critical | The `finalized` block has stopped advancing while the head moves. | The chain's finality, or every provider's view of it. Withdrawals are slow; nothing is lost. |
| `custody_connect_failed` | Critical | At start, the adapter could not connect. | [restart-and-recovery.md](restart-and-recovery.md). |
| `deposit_rejected`, `custody_deposit_rejected`, `house_deposit_rejected`, `deposit_unrepresentable`, `deposit_reversed` | Critical | A deposit was not credited, or a credited one was proved reversed by the quorum. | Read the `reason`; none can be fixed on the node. Call the owner. `deposit_reversed` debits the account once. |

### Quorum lost

1. Test each provider yourself, outside the node, from a machine other than the node's host:

   ```sh
   for url in <provider urls>; do
     echo "$url: chain $(cast chain-id --rpc-url $url)  head $(cast block-number --rpc-url $url)  finalized $(cast block finalized --rpc-url $url -f number)"
   done
   ```

   Chain ids must all be `42161`. Heads within a few blocks. Finalized within a batch or so. A
   provider that is far behind, on another chain, or erroring is the fault.
2. Compare one answer they should agree on, for a block all of them have:
   `cast call $VAULT 'totalDeposited()(uint256)' --block <finalized block> --rpc-url <each>`.
   All must print the same number.
3. If the providers themselves are healthy but the node cannot reach them, the cause is the node's
   network or its egress rules: the host's side.
4. The quorum restores by itself when enough providers answer identically; the alert resolves.
5. Replacing a provider is a new release. Pick one run by a different operator from the others: the
   quorum protects against one provider lying only if the others are independent of it. Providers
   must serve state back to `custody.evm.start_block` (archive access).

A provider's key is a delivered secret, not part of the config: rotating one needs a delivery and a
restart, and no release. See [key-recovery.md](key-recovery.md), "Secrets".

## Checkpoint endpoint

`[checkpoints].rpc_url` is a separate endpoint from the custody providers. It carries the relayer's
transactions: it must support `eth_sendTransaction`, `eth_getTransactionByHash` and `eth_getTransactionCount`, hold the funded
relayer account, and serve historical nonce reads. It has no authority over funds.

If it is down or the relayer cannot pay, posts fail with `checkpoint_post_failed` and the checkpoints
queue (the queue is bounded; a full queue raises `checkpoint_backlog`). Trading continues, and no
withdrawal is authorised until posting resumes.

1. Check the endpoint: `cast block-number --rpc-url <checkpoint endpoint>` and
   `cast balance <relayer> --rpc-url <checkpoint endpoint>`. The relayer needs gas on Arbitrum One;
   with `[checkpoints.relayer_gas]` set, `checkpoint_relayer_low` warns below its `min_balance`, read
   at a block the custody providers agree is final, and `engine_checkpoint_relayer_balance_eth`
   exports the balance.
2. Fund the account or restore the endpoint. The queued checkpoints post in order when it works.
3. If the endpoint is gone for good, a new one with a funded relayer is a config change and a new
   release. Mind the clock: while checkpoints are not accepted, `staleAfter` (deposits refused) and
   `exitPeriod` (the permanent exit) run (reference.md, "Deadlines that run while the node is down").
4. A stuck fee or foreign pending work: [withdrawal-stalls.md](withdrawal-stalls.md),
   "`checkpoint_backlog`".

## The access source

Eligibility declarations, orders, withdrawals and custody credits depend on a signed screening
answer from the access source (`[access]`). When it cannot be reached or its answer is stale, the
affected action is refused and nothing else changes: `access_source_failed` (Warning) is raised and
the request stays unexecuted; `access_deposit_retry_failed` (Warning) means pending deposits stay
uncredited because the retry cannot screen; `access_deposit_held` (Critical) is a deposit held by a
screening decision, which only the source's later decision can release.

Restore the source or its credentials. The endpoint URL is in the config (a new release); its signing
authority is recorded in the log and cannot be replaced without a fresh log.
