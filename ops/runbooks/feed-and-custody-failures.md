# Feed and custody failures

Use this runbook when a service outside the enclave that the trading server depends on fails: a
price feed, a source of benchmark rates, the trusted time servers, a custody RPC provider, the
checkpoint endpoint, or the access source. Each of these fails closed. A market with no price
executes nothing, a custody quorum that disagrees credits nothing, and a clock that cannot be
trusted stamps nothing. None of these failures loses an instruction that the log has recorded.
Commands, configuration keys and their output call a running trading server a node.

- [Price feeds](#price-feeds)
- [Benchmark rates](#benchmark-rates)
- [Trusted time](#trusted-time)
- [Custody RPC providers](#custody-rpc-providers)
- [Checkpoint endpoint](#checkpoint-endpoint)
- [The access source](#the-access-source)
- [How a price exists](#how-a-price-exists)

The steps use the shell variables that [Variables](reference.md#variables) sets up.

**Every configuration change in this runbook is a new release.** The venues, symbols, providers,
endpoints and relayers are in the configuration file, whose hash is part of the release's identity,
and a running trading server cannot change them. Plan a replacement as an upgrade, as
[Deploy and rollback](deploy-and-rollback.md) describes, not as a quick fix.

## Price feeds

A market has a price only while enough of its venues quote it, recently and in agreement, as
[How a price exists](#how-a-price-exists) describes. Start with
[Find what is down](#find-what-is-down), which matches each feed alert to its cause.

### Find what is down

Use this procedure when a feed alert fires, or when a market that should be open has no price.

1. List each market's pricing state from the metrics:

   ```shell
   curl -s http://<metrics host>:9464/metrics | grep -E '^engine_market_priced'
   ```

   `<metrics host>` is the address of the trading server's metrics listener. Each line reads `1` for
   a market with a usable price and `0` for a market without one.

2. List the markets that are open:

   ```shell
   curl -sf $API/v1/markets | jq -r '.[] | select(.open) | "\(.id) \(.symbol)"'
   ```

   An open market whose `engine_market_priced` reads `0` is the `market_no_price` case, which
   [A market has no price](#a-market-has-no-price-market_no_price) covers. A market's `mark` in
   `/v1/markets` is the last stored price, and it remains after live pricing is lost. A missing
   `mark` therefore shows only that a market has never had a price.

3. Match the firing alerts to their cause:

   | Alert | Level | Meaning | What to do |
   | --- | --- | --- | --- |
   | `venue_feed_down` | Warning | A connection to a venue or a data vendor is down, and the trading server reconnects with a backoff from `reconnect_min` to `reconnect_max`. For CME Group, the alert also fires on a reconnect that has no bearer token to connect with. Its fields are `venue` and `feed`. | Check the vendor's status page and the account's plan, key, entitlement and connection limit. For CME Group, see [CME Group's feed](#cme-groups-feed). For Alpaca, see [Alpaca's feed](#alpacas-feed). Nothing needs to be done on the trading server. |
   | `venue_symbol_silent` | Warning | A symbol has had no fresh quote for `feeds.venues.timeout` while its market is open. The trading server subscribes to the symbol again, and again after each further `feeds.venues.timeout` of silence, with a backoff from `reconnect_min` to `reconnect_max`, until the symbol quotes. On Coinbase, the symbol is subscribed again alone on its connection each time, so the other symbols keep quoting. On other venues, the connection is replaced, which costs each of its other symbols a reconnect. | Check that the symbol is still the vendor's, since a ticker or contract may have changed. A wrong symbol is a configuration fix. |
   | `venue_stale` | Warning | A venue has kept sending a symbol's books that were already too old to count when it sent them, for 2 seconds, or for `feeds.venues.timeout` if that is shorter. The venue is streaming that symbol from behind, and the message gives the age. The symbol is subscribed again at once, on Coinbase first alone on its connection and then on a new connection. It is subscribed again after each further 2 seconds of such books, with a backoff from `reconnect_min` to `reconnect_max`. The alert resolves at the next book young enough to count. Its fields are `venue`, `feed`, `symbol` and `market`. | Check the venue's status page. While the other source quotes, nothing needs to be done on the trading server, but the market has no price until this source is current. |
   | `venue_token_failed` | Warning | CME Group's token endpoint failed or refused a request for a bearer token. While the connection's current token has life left, the feed still prices. The alert resolves at the next request the endpoint grants. Its fields are `venue` and `feed`. | See [CME Group's feed](#cme-groups-feed). |
   | `venue_message_refused` | Warning | The trading server refused a vendor's message because it was malformed or unknown, or CME Group or Alpaca refused a login or a subscription. The alert fires at most once a minute, with a count. For a venue that holds no credential, the message is quoted in part. For a venue that holds one, such as Alpaca, CME Group, Massive or TraderMade, the message is identified by its length and a prefix of its SHA-256, because it could echo the credential. | Read the reason and the excerpt, or tell messages apart by their hashes. A change in a vendor's format needs a new release of the trading server. For refusals by CME Group, see [CME Group's feed](#cme-groups-feed). For refusals by Alpaca, see [Alpaca's feed](#alpacas-feed). |
   | `venue_clock_ahead` | Warning | A venue's timestamps are well ahead of the host's clock. | Check `clock_drift` and the host's clock. Quotes ahead of sequenced time are refused, and `prices_rejected` fires with the reason `quote_ahead`. |
   | `venue_delayed` | Warning | A connection's pings show that its stream may be further behind than `feeds.venues.max_delay`. What it reads then counts as no quote, and a market that needs that venue has no price. The alert resolves when a frame next counts. Its fields are `venue` and `feed`. | Check the network path to the venue, and the host's load. If the path is healthy, the host may be holding the stream back, as [Incident response](incident-response.md) describes. |
   | `prices_rejected` | Warning | The engine refused a price update. The `reason` is one of `unknown_market`, `malformed_quote`, `quote_ahead`, `market_closed`, `contract_not_taken` or `other`. | For `contract_not_taken` on a futures market, see [Futures markets](#futures-markets). |
   | `market_no_price` | Critical | An open market that a feed prices has had no usable price for `alerts.no_price_after`, which is 5 seconds in the template. The time counts from trusted time's first sample of the run. | Follow [A market has no price](#a-market-has-no-price-market_no_price). |
   | `market_unfed` | Warning | No configured feed prices the market, so it cannot be traded. It never raises `market_no_price`. | This is expected only for a market that has not been given a feed. Otherwise it is a gap in the configuration. |
   | `sequencer_stalled` | Critical | The sequencer answered no monitor sample for `alerts.no_price_after`. | Follow [Restart and recovery](restart-and-recovery.md). |

### A market has no price (`market_no_price`)

Use this procedure when `market_no_price` fires. An open market has had no usable price for
`alerts.no_price_after`. No order in the market can be executed, including an order that closes a
position, and positions in it that fall below the close-out level wait until the market has a price
again.

1. Read which sources are missing from the alert's text, which says, for example,
   `only 1 fresh sources, need 2`. Match the missing sources to `venue_feed_down` or
   `venue_symbol_silent` for that market.

2. Check the market's calendar. A market that is closed on its calendar is not a fault, because the
   alert covers open markets only. Then check whether the venue announced a closure for today. A
   closure announced less than seven days ahead cannot go on the calendar, because a calendar change
   closes a listed market only from seven days after it is logged, so the alert fires for the length
   of such a closure. Once the venue's announcement confirms it, the alert is expected: go to step 5,
   and add the closure to the calendar only for dates at least seven days ahead.

3. If one vendor is down, wait for its reconnect, check its status page, and check your account with
   the vendor. If the vendor will be down for a long time, the owner decides whether to ship a
   release that prices the market from another source, which is a configuration change, or to leave
   the market without a price. The books are safe either way, because a position in a market with no
   price waits.

4. If both sources quote but disagree by more than the market's divergence limit, the engine refuses
   the price on purpose. Compare the two quotes.

   **Do not widen `max_divergence_bps` to make the alert go away.**

5. Consider making the market reduce-only while it has no price, so that no new position is opened
   the moment it has a price again, as [Make markets reduce-only](reduce-only.md) describes. That
   decision is the owner's, and it is not required.

**How to tell it is resolved.** The alert resolves, and `engine_market_priced` reads `1` for the
market.

**When to escalate.** Tell the owner when a vendor will be down for long enough that a release, or a
market left without a price, has to be decided.

### A crypto venue is down or quiet

Use this procedure when `venue_feed_down`, `venue_symbol_silent` or `venue_stale` names Coinbase or
Kraken, or when `market_no_price` fires for BTC-USD, ETH-USD or SOL-USD.

BTC-USD, ETH-USD and SOL-USD, markets 1, 2 and 3, are priced from Coinbase and Kraken only, with
`min_sources = 2` and `max_age_ms = 1000`. If either venue goes down on all of its connections, all
three markets have no price. No order can be executed and no position can be closed out until the
venue is back, so trading in those markets stops.

Each venue is read on two connections, and each venue's two connections count as one source.
Coinbase is read on Advanced Trade and on Exchange, whose venue is `coinbase_exchange`. Kraken is
read on two connections to its v2 feed. One connection failing costs nothing, because the engine
holds the later-stamped of a source's quotes.

A book that does not change is kept quoting by the connection's pings for about five seconds after
its last change, and for about a second once the venue stops answering the pings. A symbol whose
book shows no change for more than about six seconds therefore stops its own market. A venue can
also stream one product minutes behind while the others on the same connection are current. In that
case `venue_stale` names the product and the age of its quotes, and the trading server subscribes to
the product again alone, then on a new connection if it stays stale, repeating with a backoff until
the product is current.

**This is the failure to watch most closely.** BTC-USD and ETH-USD charge 1% margin on the first $2
million of a position, which allows leverage of up to 100 times from volume tier 2. At that leverage
a position reaches its close-out level, half of its margin, about 0.5% from its entry, and it loses
more than its balance after a move of about 1%. Any loss beyond an account's balance falls on the
house, and `bad_debt` fires. While a market has no price in a fast move, nothing in it can be closed
out, and when the price returns the whole gap is taken at once. Read the margin terms in force from
`margin_tiers` in `/v1/markets`.

You can see the outage in these signs:

- `venue_feed_down` or `venue_symbol_silent` fires as a Warning, naming the venue;
- `market_no_price` fires as a Critical after `alerts.no_price_after`;
- `engine_market_priced{market="1"}`, `{market="2"}` and `{market="3"}` read `0`;
- executions in the three markets stop. A market's `mark` in `/v1/markets` remains after live
  pricing is lost, so it does not show that the market has a price.

1. Make BTC-USD, ETH-USD and SOL-USD reduce-only at once, as
   [Make markets reduce-only](reduce-only.md) describes.

   **Changes the live system.** The owner must approve the administrative instruction and its scope.

   Reduce-only stops new risk from the first execution after the price returns. It closes nothing,
   and nothing can be closed while there is no price.

2. Find out which venue is down, and why. Check the venue's status page, your account or IP limits
   with the venue, and DNS and outbound traffic from the host. The trading server reconnects by
   itself, with a backoff from `reconnect_min` to `reconnect_max`, which is 30 seconds in the
   template.

3. Tell the owner the time the market lost its price, the venue, and the house's net asset value,
   which is `nav` in `/v1/house`:

   ```shell
   curl -sf $API/v1/house | jq .nav
   ```

There are three things that you cannot do during the outage:

- You cannot set or override a price. No instruction does.
- You cannot lower `min_sources` or `max_age_ms`, or add a third source, as a quick fix. The venues
  are in the configuration, so a change to them is a new release, as
  [Deploy and rollback](deploy-and-rollback.md) describes. The price rules are market settings that
  the owner changes by an administrative instruction.
- You cannot close positions, or force a close-out, without a price. Raising the stress floor does
  not help either, because it widens the spreads on prices that do not exist.

**How to tell it is resolved.** `engine_market_priced` reads `1` for all three markets, and the
alerts resolve. Then lift reduce-only, as
[Resume normal trading](reduce-only.md#resume-normal-trading) describes.

**When to escalate.** The books are safe throughout. What is at stake is the house's exposure to the
price gap. Keep the time the price returned, and any `bad_debt` pages after it, for the owner.

### Futures markets

Use this procedure when `futures_roll_overdue` or `futures_last_contract` fires, or when
`prices_rejected` fires with the reason `contract_not_taken` on a futures market. An index or
commodity market is priced from its front futures contract, and rolls onto the next contract at the
time its listing names.

- **`futures_roll_overdue` (Warning).** The roll time has passed, and no price gap between the two
  contracts has been recorded. The roll records the gap between the two contracts' prices, so it
  needs quotes for the next contract. Check the feeds that quote the **next** contract.

- **`futures_last_contract` (Warning).** The market is priced from the last contract that its
  listing names. Before that contract expires, the owner lists the next contract.

  1. Check that the vendors' symbols for the new contract are in the configuration. If they are not,
     adding them is a new release.

  2. Write a file holding the keys of a `[[genesis.markets]]` entry, without the table header, with
     the market's whole specification and the new `contracts`.

  3. Send the listing:

     ```shell
     $ENGINE admin --config $CONFIG --node $API --pin $PIN upsert-market <market.toml>
     ```

     `<market.toml>` is the file from step 2. The last line of the output is
     `[{"type":"admin_applied"}]`.

     **Changes the live system.** The owner must approve the exact specification.

  While parameter votes govern, as [Parameter votes](operator-candidate.md#parameter-votes)
  describes, the engine refuses this instruction from the operator. The members of the rotation set
  propose the new listing as a normal vote instead, which applies seven days after the engine logs
  it.

  **Propose the new listing at least seven days before the current contract's roll time, plus the
  time the vote takes to pass. Otherwise the market has no price once the current contract
  expires.**

- **`prices_rejected` with `contract_not_taken`.** A quote arrived for a contract that the market is
  not taking. This is expected around a roll. If it persists, the feed's symbols and the listing's
  contracts disagree.

### CME Group's feed

Use this procedure when `venue_token_failed` fires, or when `venue_message_refused` or
`venue_feed_down` names CME Group, whose venue is `cme`.

The launch configuration has no CME Group connection: markets 20 to 22, 25 and 30 to 32 are priced
from Massive's futures feed alone (owner's decision of 2026-11-20), so these alerts do not fire on a
launch node. For those markets, `venue_feed_down` naming Massive means no price at all, because
`min_sources = 1` and Massive is their only source. This procedure applies only to a configuration
that adds a CME Group connection. Such a connection's handshake carries a bearer token. The trading server fetches the token from
CME Group's token endpoint with the API ID and its password, which are the delivered secrets that
the connection's `oauth` names. Without both secrets, the connection stays off under
`secret_missing`.

Three quarters of the way through a token's life, the trading server fetches the next token while
the connection streams, opens a second connection with it, and switches to the second connection
once it is subscribed. CME Group's quotes therefore continue, and the switch raises nothing. If the
second connection fails, the first streams on, and the trading server tries again with a new token
after a backoff, until the first connection's token expires.

Match the alert to its case:

- **A failed or refused request for a token.** The first failure raises `venue_token_failed` with
  `venue = "cme"`, giving the endpoint's answer by its HTTP status and OAuth error code, such as
  `token request refused (401 Unauthorized): invalid_client`. The endpoint's own text is not logged.
  While the connection's token still has life, the feed keeps pricing and retries the request with a
  backoff, so the alert gives warning before the token expires. With CME Group's 30-minute tokens,
  refreshed at three quarters of their life, the warning comes about seven minutes ahead. Once the
  token expires, the connection ends, and the reconnect, having no token, also raises
  `venue_feed_down`. The next request the endpoint grants resolves the alert.

  1. Check that the API ID is entitled to the data.
  2. Check that the delivered values are for CME Group's production environment, because CME Group
     issues them for each environment separately.
  3. If the values are wrong, deliver new ones, as [Secrets](key-recovery.md#secrets) describes. The
     trading server takes them at its next restart.

- **A refused login.** CME Group answers `LOGINERR`. `venue_message_refused` fires with
  `login refused`, followed by `venue_feed_down`, as above. Each new attempt at a token switch
  fetches its own token. So does each reconnect, except one that opens with a token that a switch
  fetched ahead.

- **A refused subscription.** CME Group answers `SUBSCRIPTION_ERROR`, and `venue_message_refused`
  names each error's code, such as `subscription refused: ERROR_400`. CME Group's message text is
  not logged. The connection is replaced once it has gone `feeds.venues.timeout` without a fresh
  quote.

  The trading server subscribes to each product whole and ignores the contracts that no symbol
  names, so a contract that CME Group has delisted cannot cause the rest to be refused. A symbol
  names a contract by its product code and period, such as `ES:202612`, and a wrong symbol is a
  configuration fix, which is a new release. CME Group's documentation lists the codes `ERROR_400`,
  `ERROR_401` and `ERROR_403`, and does not define the last two. Take those to CME Group with the
  API ID. The trading server subscribes to a connection's products in one request, so a refusal over
  one exchange's entitlement can cost every product on the connection.

- **A refused top of book.** A `TOB` message is refused whole when it names a configured contract
  under an instrument that is not the contract's outright, such as a calendar spread sharing its
  period (`ESZ6-ESH7`), or when it names a contract twice. `venue_message_refused` names the
  contract and the message's length and hash, and the trading server takes no price from the
  message. Take the time and the contract to CME Group. A future of a subscribed product whose
  `periodCode` is not of the form `YYYYMM` is refused in the same way, naming the product. CME
  Group's own message examples send no such future, so such a message is a shape that CME Group's
  field table does not promise.

**How to tell it is resolved.** The alerts resolve.

**When to escalate.** Take refusals with undefined codes, and refused top-of-book messages, to CME
Group with the API ID.

### Alpaca's feed

Use this procedure when `venue_message_refused`, `venue_feed_down` or `venue_symbol_silent` names
Alpaca.

Markets 40 to 59, the US shares, are priced from Massive's stocks feed and from Alpaca's
consolidated feed, `/v2/sip`, which carries the quotes of every US exchange on Alpaca's Algo Trader
Plus plan. An Alpaca connection logs in with the API key ID and the secret key, which are the
delivered secrets that its `key_pair` names. Without both secrets, the connection stays off under
`secret_missing`, and the shares have Massive as their only source, which is no price at
`min_sources = 2`. Alpaca's errors are logged by their code and Alpaca's meaning for the code, such
as `error 402: auth failed`. Alpaca's own text is not logged.

Match the alert to its case:

- **A refused login.** `venue_message_refused` fires with `error 402: auth failed`,
  `error 404: auth timeout` or `error 409: insufficient subscription`, followed by `venue_feed_down`
  as the trading server reconnects with a backoff.

  1. Check that the delivered values are the account's live keys.
  2. Check that the account's plan includes the consolidated feed, which is Algo Trader Plus. Alpaca
     refuses the login for a plan without it.
  3. If the values are wrong, deliver new ones, as [Secrets](key-recovery.md#secrets) describes. The
     trading server takes them at its next restart.

- **`error 406: connection limit exceeded`.** An Alpaca account streams on one connection at a time,
  and another process holds a connection with the same keys. The other process may be a second
  trading server, a test client given the production keys, or the trading server's own earlier
  connection that Alpaca has not yet seen close, which a reconnect outlasts. Find and stop the other
  process.

  **Never give the trading server's keys to anything else.**

- **A refused subscription.** Alpaca answers `error 405: symbol limit exceeded` or
  `error 410: invalid subscribe action for this feed`, or confirms a subscription that leaves
  symbols out, as in `the subscription confirms quotes for 18 of 20 symbols`. The connection stays
  up. While the market is open, each symbol that the connection does not quote raises
  `venue_symbol_silent` after `feeds.venues.timeout`, and the connection is replaced in order to
  subscribe again. Check the plan and the symbols. A wrong symbol is a configuration fix, which is a
  new release.

- **A refused quote.** A quote whose prices, sizes or time do not parse, or a quote for a symbol
  that the connection did not subscribe to, is refused alone, and the other quotes in Alpaca's
  message are read. `venue_message_refused` names the reason and, for a configured symbol, the
  symbol. A side of a quote with a zero price and a zero size quotes nothing, and that quote gives
  no book, without an alert.

- **A market closure.** Alpaca documents no heartbeat, and what it sends overnight and at weekends
  has not been measured. Once its subscription is confirmed, a connection whose markets are all
  closed stays up with no read deadline, and pings still end a dead connection. At each market open,
  the connection has a whole `feeds.venues.timeout` from the open. A connection that is still silent
  then raises `venue_symbol_silent`, and is replaced with `venue_feed_down`. A login or subscription
  that stalls through a closure is replaced, as it would be at any hour. Massive's stocks connection
  is kept in the same way.

**How to tell it is resolved.** The alerts resolve, and `engine_market_priced` reads `1` for markets
40 to 59 while their exchange is open.

### Tightening `max_age_ms`

Use this procedure when the owner has decided to shorten a market's `max_age_ms`, the age beyond
which a quote stops counting towards the market's price.

The engine checks the change, when it is sequenced, against the quotes that the market holds. While
the market has open positions, or a market quoted in the currency it converts has open positions,
the change applies only if those quotes, at their age at that moment, still price the market under
the shorter limit. Otherwise the engine refuses the change with `price rules it cannot price under`.
For a held market that converts at the market being changed, the reason is
`which the price rules cannot price under`.

A venue's quote keeps the venue's own timestamp, and the feed's repeat rule only delays a repeat of
an unchanged price. A quiet mid price therefore leaves quotes older than a tighter limit, even while
the venues are healthy.

A tightening applies at once. Undoing it is a loosening, which applies only seven days after a
listing notice, or by a vote, and never beyond the market's launch rules. Tighten only what the
owner accepts for at least seven days.

While parameter votes govern, which is while rotation is on and a rotation set with members is in
effect, as [Parameter votes](operator-candidate.md#parameter-votes) describes, the engine refuses
the operator's `upsert-market`. Each change in the steps below is then proposed as a normal vote,
which applies seven days after the engine logs it, and is checked against the quotes the market
holds at that time. A voted change that the engine refuses at that time is spent, and
`proposal_rejected` fires. It cannot be signed again, and only a new proposal, with its own seven
days, can make the change. Each step of a stepwise cut is a vote of its own. The reduce-only
instructions in step 5 remain the operator's, and they must be in force before the change's
`applies_at` in `GET /v1/house/state`.

1. Check that the market is open and has a price, which is when `engine_market_priced` reads `1` for
   it, as [Find what is down](#find-what-is-down) shows. A closed market holds old quotes, and every
   tightening is refused until it opens. `market_no_price` fires only after `alerts.no_price_after`,
   so its silence proves nothing.

2. Sign the change with `upsert-market`, as [Futures markets](#futures-markets) shows.

   **Changes the live system.** The owner must approve the exact specification.

   If the engine refuses the change, wait for the venues to quote again, which moves the mid price
   or triggers the repeat, and sign the same change again.

3. For a large cut, tighten in steps, signing each step after the venues have quoted again, so that
   each step still prices the market from quotes that arrived after the previous step.

   **Changes the live system.** Each step is its own `upsert-market`, and the owner must approve the
   exact specification of each.

If the change is still refused, follow steps 4 to 10. In these steps the check against the market's
quotes does not apply, so the market may be left without a price.

4. Ask the owner to accept that the market may stay reduce-only, or without a price, for seven days.
   If the tighter limit does not price the market, restoring the previous limit is a loosening,
   which takes that long.

5. Make every held market that the refusal names reduce-only, as
   [Make markets reduce-only](reduce-only.md) describes. Those are the market being changed and any
   market that converts at it.

   **Changes the live system.** The owner must approve the reduce-only instructions, the change, and
   the steps that follow.

6. Sign the change again. The check does not apply to a market that is reduce-only.

7. In the same window as the change, give notice of the restore, so that the previous limit can
   apply seven days later if it is needed:

   ```shell
   $ENGINE admin --config $CONFIG --node $API --pin $PIN announce-listing <market> \
     --price-rules-from <spec.toml> --effective-at <time>
   ```

   `<market>` is the market's id, and `<spec.toml>` is a market specification carrying the previous
   price rules. `<time>` is in RFC 3339 form, such as `2027-01-01T00:00:00Z`, at least seven days
   after the notice is logged. The last line of the output is `[{"type":"admin_applied"}]`.

   **A listing notice replaces the market's pending notice, whatever that notice names.**

8. Wait for a monitor sample taken after the change. The gauge samples once a second, and keeps its
   last value when a sample times out, so a `1` read at once can describe the previous limit. Then
   check that `engine_market_priced` reads `1` for the market you changed, and for each market that
   converts at it **and is open**. A market closed on its schedule reads `0` whatever the limit, so
   check such a market at its next session. The `mark` in `/v1/markets` remains after pricing is
   lost, so it proves nothing.

9. If a market reads `0`, restore the previous limit once the notice from step 7 allows it, with an
   `upsert-market` carrying the previous price rules. Then wait for a monitor sample taken after the
   restore, and for the venues to quote again, because loosening discards the quotes that were stale
   under the tighter limit.

   **Changes the live system.** The owner must approve the restore.

10. Resume normal trading only once `engine_market_priced` reads `1` for the market, as
    [Resume normal trading](reduce-only.md#resume-normal-trading) describes.

**How to tell it is resolved.** The change has applied, and `engine_market_priced` reads `1` for the
market and for each open market that converts at it.

## Benchmark rates

Use this procedure when one of the benchmark alerts below fires. Financing uses dated benchmark
rates, which a separate worker signs and sends, as
[The benchmark worker](deploy-and-rollback.md#the-benchmark-worker) describes. Benchmark rates do
not affect prices.

Each daily rollover takes the latest rate observed on or before its local date, and weekdays here
are Monday to Friday. A failed fetch from a publisher keeps every rate already sequenced. It still
counts as attempted, so it holds back neither the other benchmarks nor a start.

| Alert | Level | Meaning | What to do |
| --- | --- | --- | --- |
| `benchmark_fetch_failed`, `benchmark_sync_failed`, `benchmark_schedule_watch_failed` | Warning | A fetch from a publisher, or a call to the trading server, failed. The worker retries by itself. | Check the publisher's endpoint and the worker's host. |
| `benchmark_rate_refused` | Warning | A published rate is more than 300 bps a day away from the applied rates next to its date, or it differs from a date already applied. The worker leaves the rate out and signs the rest, so nothing else waits on it, and the last applied rate keeps financing. The limit grows with each day, so a real move lands once it fits, but a revision of an applied date never does. | Check the rate against the publisher. Signing the rate by hand is refused for the same reason. |
| `benchmark_rate_stale` | Critical | At a rollover, the newest observed rate is older than five weekdays. Financing still applies. | Fix the worker and its fetch from the publisher. |
| `benchmark_rate_missing` | Critical | A benchmark has no observed rate, so no financing is charged on its markets. | Fix the worker and its fetch from the publisher. |
| `benchmark_rate_undated` | Critical | A legacy rate with no date still applies. | Fix the worker and its fetch from the publisher. |
| `benchmark_sync_pending` | Warning | The trading server is waiting for the worker's signed history before it catches up. | Start the worker, and see the steps below if the operator key is gone. |
| `benchmark_sync_skipped` | Warning | The registry installed an operator in place of the key that the log records. | See the steps below. |

For `benchmark_rate_stale`, `benchmark_rate_missing` and `benchmark_rate_undated`, the owner may set
a rate by hand as a last resort:

```shell
$ENGINE admin --config $CONFIG --node $API --pin $PIN set-benchmark-rate <benchmark> <annual bps>
```

`<benchmark>` is the benchmark's name, and `<annual bps>` is the rate in basis points a year. The
last line of the output is `[{"type":"admin_applied"}]`.

**Changes the live system.** This is an administrative instruction and a financing decision for
every holder of the benchmark's markets, and the owner must approve it.

When `benchmark_sync_pending` fires and the operator key is gone, no worker can sign:

1. If rotation is on, run the whole fallback that
   [When no standby qualifies](standby.md#when-no-standby-qualifies) describes. The registry
   installs an operator only while rotation is off, and turning rotation off also withdraws the
   release that the registry named.
2. Once the registry's installation of another key is final, restart the trading server, as
   [Restart and recovery](restart-and-recovery.md) describes. The trading server then ends the hold
   at its first stamp.

   **Changes the live system.** A restart needs the owner's approval.

When `benchmark_sync_skipped` fires, the registry has installed an operator in place of the key that
the log records, which the fallback presumes is gone. The installed key signs nothing until seven
days after the engine logs its mirror. Waiting for a snapshot of the rates would also hold back that
mirror, so the trading server sequences without one, and financing uses the last applied rates. The
alert resolves when a snapshot applies.

1. Read the effective time of the pending rotation:

   ```shell
   curl -sf $API/v1/operator | jq .pending_rotation
   ```

2. Once that time has passed, start the worker under the installed key. From then on, a hold waits
   for its snapshot.

## Trusted time

Use this procedure when a Roughtime alert, `clock_drift` or `host_clock_behind` fires.

Every stamp is checked against signed Roughtime readings from independent servers. Nothing is
sequenced before the first sample of the run, so no market raises `market_no_price` before it.
Sequencing halts when the host's clock is outside `time.max_drift` of the readings, which is 15
seconds in the template, or when no sample has arrived for 60 seconds. A halt does not hold
`market_no_price` back, so each market raises it while its feeds have not priced it again.

| Alert | Level | Meaning | What to do |
| --- | --- | --- | --- |
| `roughtime_server_failing` | Warning | One server's queries keep failing. | Check the UDP network path to the server. A majority of the servers must agree, so one failing server uses up the slack. |
| `roughtime_server_disagrees` | Warning | One server signed a time outside the agreed sample. | Do not act on one server. If it persists, the owner may remove the server, which is a new release. |
| `trusted_time_no_sample` | Warning | No sample could be taken. | Check outbound UDP traffic to the servers, and the host's resolver. |
| `trusted_time_unavailable` | Critical | No sample arrived within the 60-second grace, and sequencing has halted. | As for `trusted_time_no_sample`. Sequencing resumes by itself when a sample arrives. |
| `host_clock_behind` | Warning | The host's clock has fallen a second behind monotonic time. Stamps keep monotonic time's pace, so quotes still age out. A clock that ran ahead and came back leaves stamps ahead of it, and markets may have no price until that gap drains, at 1% of the elapsed time. | Fix the host's clock. The host controls the clock, so also read [Incident response](incident-response.md). |
| `clock_drift` | Critical | The host's clock, or a stamp, is outside the drift limit of trusted time, and sequencing has halted. | Fix the host's clock. If the clock jumped a long way, also read [Incident response](incident-response.md), because the host controls the clock. |

**How to tell it is resolved.** The alert resolves, and sequencing resumes by itself once a sample
arrives and the host's clock is within the drift limit.

## Custody RPC providers

Use this procedure when one of the custody alerts below fires.

The custody watcher reads the vault through at least three independently operated providers, listed
in `custody.evm.providers`. It accepts an answer only when a strict majority of the providers give
the identical answer, such as two of three or three of four. The watcher credits deposits and
releases principal for withdrawal from those answers, so a loss of the quorum stops crediting.
Trading continues.

| Alert | Level | Meaning | What to do |
| --- | --- | --- | --- |
| `custody_rpc_provider_failed` | Warning | One provider failed an operation. Its fields are `provider` and `operation`. | The quorum holds. Check the provider. With three providers, this uses up your last slack. |
| `custody_rpc_provider_disagreed` | Warning | One provider's answer differs from the quorum's. The provider is lagging, forked or wrong. | Compare the provider by hand, as [Quorum lost](#quorum-lost) shows. Do not ignore the alert for long, because it is also what a provider run by an attacker looks like. |
| `custody_rpc_quorum_lost` | Critical | No answer has a majority. When the alert's `quorum` field is `custody`, the deposit watcher's quorum, the alert is Critical and crediting is paused. When it names any other reader, the alert is a Warning, and only that reader is paused, as described below the table. | Follow [Quorum lost](#quorum-lost). For the operator list, see `operators_read_failed`. For the log-head read, see `log_head_read_failed` or `log_heads_startup_failed`. |
| `operators_read_failed` | Warning | The operator list, `GET /v1/operators`, cannot be read at a final block. No call to the registry's lens has a majority, or the chain or token is wrong, or the lens is inconsistent. The `operation` field names the call, which is `lens_registry`, `lens_overview` or `lens_candidates_<page>`. The route serves the last list until `custody.evm.finality_stalled_after`, and then answers 503. Nothing else is paused. | Check the providers as above, and check that `custody.evm.staking.registry.lens` is the `RegistryLens` beside the configured registry. A provider that caps the gas of an `eth_call` fails on the larger pages. |
| `custody_rpc_failed`, `custody_chain_unsettled` | Warning | A poll failed on a read, or the chain's answers are lagging. The watcher retries at every poll interval. | Look at the providers. If the alert lasts for `custody.evm.stall_critical_after`, `custody_deposits_stalled` follows. |
| `custody_chain_inconsistent` | Critical | The chain's answers contradict the configuration or the cursor: a different chain, vault or token, or a final block that moved back. | **Do not change anything.** This is not an outage. Call the owner. |
| `custody_deposits_stalled` | Critical | No vault deposit has been read for `custody.evm.stall_critical_after`. | Work through the rows above, because deposits are not being scanned. A deposit that a checkpoint covers and that stays uncredited this long also pauses withdrawals, as [Withdrawal stalls](withdrawal-stalls.md) describes. |
| `custody_deposits_lagging` | Critical | Deposit reads have trailed the final block for `custody.evm.stall_critical_after`. | The providers are slow or rate-limited, or the range of blocks per request, `custody.evm.max_block_range`, or the poll interval does not suit them. |
| `custody_head_stale` | Critical | An endpoint's newest block is old, so its head is not advancing. | The provider, or the chain itself, has stopped. |
| `custody_finality_stalled` | Critical | The `finalized` block has stopped advancing while the head moves. | The chain's finality has stalled, or every provider's view of it has. Withdrawals are slow, but nothing is lost. |
| `custody_connect_failed` | Critical | At start, the custody adapter could not connect. | Follow [Restart and recovery](restart-and-recovery.md). |
| `deposit_rejected`, `custody_deposit_rejected`, `house_deposit_rejected`, `deposit_unrepresentable`, `deposit_reversed` | Critical | A deposit was not credited, or the quorum proved that a credited deposit was reversed. | Read the `reason`. None of these can be fixed on the trading server. Call the owner. `deposit_reversed` debits the account once, except for a receipt that moved to its replacement in the same command, whose cash nets out. A reversed deposit that the chain includes again is credited again, less the write-offs against the account since the reversal that no other returning deposit has repaid, at most the deposit's principal. A protocol activation to V83 or later while the custody watcher is moving a reorganisation's deposits one at a time can raise one `custody_deposit_rejected` with reason `conflicting_receipt` for that move or credit; the watcher's next poll moves the deposits together. If the node log then shows `reorganisation moved its receipts together` and no balance changed, nothing more is needed. |

The `quorum` field of a custody quorum alert names the reader whose quorum was lost. The deposit
watcher's quorum, `quorum=custody`, feeds credits and checkpoints, so its loss is Critical and
pauses crediting. Every other reader feeds neither, so it raises its quorum alerts as Warnings, and
a lost quorum pauses only that reader. These are the readers that an operator meets most often:

- `quorum=operators` pauses only the operator list;
- `quorum=log_heads` pauses only the log-head read;
- `quorum=relayer_balance` pauses only the read of the checkpoint relayer's balance;
- `quorum=staking_terms` pauses only the check of `InitStaking` against the staking contract;
- `quorum=standby_keys`, `quorum=liveness` and `quorum=seed_relay` pause only the reads of standby
  keys, of the liveness rule and of the seed receiver.

A rotation-set member's other readers, such as `promotion`, `vouching`, `member_link` and
`liveness_caller`, behave in the same way.

### Quorum lost

Use this procedure when `custody_rpc_quorum_lost` fires, or when a provider disagrees with the
quorum.

1. Test each provider yourself, outside the trading server, from a machine other than its host:

   ```shell
   for url in <provider urls>; do
     echo "$url: chain $(cast chain-id --rpc-url $url)  head $(cast block-number --rpc-url $url)  finalized $(cast block finalized --rpc-url $url -f number)"
   done
   ```

   `<provider urls>` are the URLs in `custody.evm.providers`, separated by spaces. Every chain id is
   `42161`. The heads lie within a few blocks of each other, and the finalized blocks within about a
   batch. A provider that is far behind, on another chain, or returning errors is the fault.

2. Compare an answer that every provider should give alike, at a block that every provider has:

   ```shell
   cast call $VAULT 'totalDeposited()(uint256)' --block <finalized block> --rpc-url <provider url>
   ```

   `<finalized block>` is a finalized block number from step 1. Run the command once for each
   provider. Every provider prints the same number.

3. If the providers are healthy but the trading server cannot reach them, the cause is on the host's
   side, in its network or its outbound traffic rules.

4. Wait for the quorum to return. It returns by itself once enough providers answer identically, and
   the alert resolves.

5. If a provider must be replaced, plan a new release. Choose a provider run by a different operator
   from the others, because the quorum protects against one provider giving false answers only if
   the others are independent of it. Every provider must serve state back to
   `custody.evm.start_block`, which needs archive access.

A provider's key is a delivered secret, not part of the configuration. Rotating a key therefore
needs a delivery and a restart, but no release, as [Secrets](key-recovery.md#secrets) describes.

**How to tell it is resolved.** `custody_rpc_quorum_lost` resolves, and crediting resumes.

**When to escalate.** Call the owner if `custody_chain_inconsistent` fires, or if a provider keeps
disagreeing with the others.

### RPC budget

The owner decided on 2026-11-20 to poll the deposit watcher every 5 s (`custody.evm.poll_interval`), and
to cut each node's calls to the custody providers in code before choosing their plans. This section
counts what one operator and two members sharing one account per provider send each provider, and says
which plans carry it. No free tier does, as the last table shows.

Every read goes to all three providers, so each provider sees every logical read once. The counts are
JSON-RPC calls per provider, measured by the harness `cargo test -p engine-node --lib rpc_budget --
--ignored --nocapture --test-threads=1`. It counts the deposit watcher's calls over a fake chain at
Arbitrum One's 4 blocks a second, with a finalized tag about 4,800 blocks behind the head, and one read
of each other reader through the real quorum while a turn of a set of three names the operator. The
seed relay and the operator list are unchanged and read from the code.

| Reader (interval) | Calls per provider per read | Per day, per provider |
| --- | --- | --- |
| Deposit watcher (5 s, `custody.evm.poll_interval`) | 39 | 673,920 |
| Liveness rule's chain read (15 s, `liveness.read_interval`, operator in a turn) | 17 | 97,920 |
| Liveness rule's member list (60 s, operator in a turn) | 10 | 14,400 |
| Log-head read-back (every 10 s while a turn names the operator) | 5 | 43,200 |
| Log-head turn read (before each post, every 20 s, in a turn) | 13 | 56,160 |
| Checkpoint relayer's balance read (5 minutes) | 6 | 1,728 |
| Fallback fixer (30 s, `liveness.fallback_interval`, every node) | 13 | 37,440 |
| Liveness caller (15 s, `liveness.read_interval`, member between turns) | 15 | 86,400 |
| Seed relay (60 s, every member) | 11 to 12 | 17,000 |
| Operator list (60 s, only with `registry.lens`) | about 12 to 20 | up to 29,000 |

The watcher's 39 calls a poll are 31 block headers, 5 `eth_call`, 2 `eth_getLogs` and one
`eth_chainId`. Twenty of the headers are the new blocks: the confirmation proof links every block's
header to the one below it up to the tag, which is how it detects a reorganisation, so this part follows
the chain at about 345,600 headers a day, whatever the poll interval. The other 11 headers and 8 calls
are fixed per poll. The proof keeps every hash of the newest 16,384 blocks, so the release, finalisation
and staking windows below the head read no header again, and the staking, registry, schedule, vote and
fault-proof mirrors read their events in one `eth_getLogs` over the window. A block whose header's logs
bloom admits a mirrored event is read again by its hash, which a block with a staking event, or a
bloom's false positive, costs. Before these changes a poll cost 101 calls: 70 headers and 20
`eth_getLogs`.

The fallback fixer reads every 30 s because anyone may fix a turn's fallback block, a draw included,
and the schedule's governance may replace the seed receiver at any time: the block's hash must then be
recorded within 256 blocks, 64 s on Arbitrum One. Once a block is fixed it reads every 5 s until the
hash is recorded.

Per provider account, a day's load is the operator in effect's readers (the watcher, the liveness rule,
the log-head reads, the relayer balance read, the fixer and the seed relay) plus the other two members'
readers (the liveness caller, the fixer and the seed relay). While a turn names the operator, the
harness counts 1,172,448 calls a day before the seed relays and the operator list: 605,664
`eth_getBlockByNumber`, 479,808 `eth_call`, 52,128 `eth_chainId`, 34,560 `eth_getLogs` and 288
`eth_getBalance`. The three seed relays add about 51,000, so the account sees about 1,223,000 calls a
day, about 14 a second on average, against 4.4 to 5.5 million before. The costs below count the seed
relays' calls at the price of an `eth_call`.

Before the registry names a schedule, as at launch, no turn names the operator and members run no
caller. The liveness rule still reads every 15 s, 9 calls a read that stop at the registry, and the
log-head poster reads the posted head and whether a turn names the operator every 30 s, 5 and 6 calls.
The harness counts 828,288 calls a day for three nodes then, about 879,000 with the seed relays.

| Provider | Published limit | The load against it | Result |
| --- | --- | --- | --- |
| Alchemy | Free: 30,000,000 compute units a month and 300 a second. Pay as you go: $0.525 per million compute units and 10,000 a second ([pricing plans](https://www.alchemy.com/docs/reference/pricing-plans)). `eth_getBlockByNumber` costs 20, `eth_call` 26, `eth_getLogs` 60 and `eth_chainId` 0 ([compute unit costs](https://www.alchemy.com/docs/reference/compute-unit-costs)) | About 28 million units a day, 840 million a month, and about 325 a second on average | The free tier does not fit. Pay as you go fits, at about $440 a month. |
| Infura | Free: 3,000,000 credits a day and 500 a second. Developer: 15,000,000 a day for $50 a month. Team: 75,000,000 a day for $225 a month ([pricing](https://docs.infura.io/get-started/pricing/), [plans](https://www.infura.io/pricing)). `eth_getBlockByNumber`, `eth_call` and `eth_getBalance` cost 80 credits, `eth_getLogs` 255 and `eth_chainId` 5 ([credit costs](https://docs.infura.io/get-started/pricing/credit-cost/)) | About 100 million credits a day | No listed plan fits. Team needs the watcher to poll about every 30 s, which brings it to about 74 million; otherwise it needs a custom plan. |
| dRPC | Free: 210,000,000 compute units per 30 days, with a dynamic limit of about 2,100 compute units a second ([free plan, 2025-05-13](https://blog.drpc.org/upcoming-changes-to-drpcs-free-plan-effective-june-1-2025/)). Pay as you go: 20 units a request, $6 per million requests ([flat pricing, 2025-05-29](https://blog.drpc.org/announcing-flat-pricing-simple-transparent-fair/)) | About 734 million units per 30 days, 37 million requests | The free tier does not fit. Pay as you go fits, at about $220 a month. |
| PublicNode | None published ([publicnode.com](https://www.publicnode.com/)) | | Cannot be budgeted. A free public endpoint can limit any client without notice, so it counts as a provider that may fail, never as one the quorum can rest on. |

Check each limit on the provider's page before relying on it, because they change.

**What fits.** The cheapest set that carries the load is dRPC and Alchemy on pay as you go, at about
$660 a month together, with PublicNode as the third provider. The quorum then rests on the two paid
providers, and loses its majority whenever either of them fails, so a second paid provider in the third
seat costs more and keeps a provider's failure from pausing crediting.

**What would lower it further.** `custody.evm.poll_interval` is the one setting left: the watcher's
fixed part is 19 calls a poll, 328,320 a day at 5 s and 109,440 at 15 s, and a deposit is credited up to
one interval later. The liveness rule and the callers, 285,120 calls a day between them, already read
at their floor of four times a 60 s heartbeat interval, which `liveness.read_interval` cannot lower.

The one header per block the watcher reads, about 345,600 a day, is the floor while the confirmation
proof links every block to detect a reorganisation. Reading fewer headers would change that check, and
needs the owner's decision.

**`max_block_range` and `stall_critical_after` at 5 s.** Both stay. At 5 s the watcher reads about 20
new blocks a poll, far under `max_block_range = 1000`, and after an outage it reads 1,000 a poll, which
is 200 blocks a second against 4 produced. The staking mirror's range read stays within it too. A
provider that refuses a range this large makes the watcher halve the range and grow it back
(`custody/sizing.rs`), so the value is a ceiling. `stall_critical_after` is 1 hour, 720 polls, so the
longer interval does not change when a stall pages.

## Checkpoint endpoint

Use this procedure when `checkpoint_post_failed`, `checkpoint_relayer_low`, `log_head_post_failed`,
`log_head_stalled` or `log_head_read_failed` fires.

`[checkpoints].rpc_url` is an endpoint separate from the custody providers, and it carries the
relayer's transactions. It must support `eth_sendTransaction`, `eth_getTransactionByHash` and
`eth_getTransactionCount`, hold the funded relayer account, and serve historical nonce reads. It has
no authority over funds.

If the endpoint is down, or the relayer cannot pay for gas, posts fail with `checkpoint_post_failed`
and the checkpoints queue. The queue is bounded, and a full queue raises `checkpoint_backlog`.
Trading continues, but no withdrawal is authorised until posting resumes.

With `[log_heads]`, the same endpoint also funds a second relayer, `log_heads.relayer`, which posts
only the log heads. It must differ from the checkpoint relayer, whose nonce tracking assumes that it
alone uses its account. The second relayer's failures raise `log_head_read_failed` or
`log_head_post_failed` as Warnings, and the poster retries. Checkpoints and trading are unaffected,
but while posts fail, the part of the log that could be cut back undetected is the whole unposted
tail. A rotating trading server, one with `operator.rotation_enabled`, must have `[log_heads]`. It
raises `log_head_stalled` after two posting intervals without an advance, not three, because its
successor checks the tail's head against the posted head at each handover, and a stalled anchor
widens the tail that the successor cannot check.

1. Check that the endpoint answers:

   ```shell
   cast block-number --rpc-url <checkpoint endpoint>
   ```

   `<checkpoint endpoint>` is `[checkpoints].rpc_url`. The command prints a recent block number.

2. Check the relayer's balance:

   ```shell
   cast balance <relayer> --rpc-url <checkpoint endpoint> --ether
   ```

   `<relayer>` is `checkpoints.relayer`, and the command prints its balance in ether. The relayer
   needs gas on Arbitrum One. With `[checkpoints.relayer_gas]` set, `checkpoint_relayer_low` warns
   below its `min_balance`, read at a block that the custody providers agree is final, and
   `engine_checkpoint_relayer_balance_eth` exports the balance. With `[log_heads]`, check
   `log_heads.relayer` in the same way, because no alert watches its balance.

3. Fund the relayer, or restore the endpoint. The queued checkpoints post in order once posting
   works.

   **Changes the live system.** Funding the relayer is a transfer, and the owner must approve it.

4. If the endpoint is gone for good, a new endpoint with a funded relayer is a configuration change
   and a new release.

   **While no checkpoint is accepted, two deadlines run: `staleAfter`, after which the vault refuses
   deposits, and `exitPeriod`, after which anyone can start the permanent exit. See
   [Deadlines that run while the trading server is down](reference.md#deadlines-that-run-while-the-trading-server-is-down).**

5. For a stuck fee, or foreign pending work at the relayer's nonce, follow
   [`checkpoint_backlog`](withdrawal-stalls.md#checkpoint_backlog). Before the next turn boundary,
   find why log-head posts fail, from `log_head_post_failed` and the second relayer's balance.

**How to tell it is resolved.** The alerts resolve, and the queued checkpoints post in order.

**When to escalate.** Tell the owner when the endpoint is gone for good, because the replacement is
a release and the deadlines above are running.

## The access source

Use this procedure when `access_source_failed`, `access_deposit_retry_failed` or
`access_deposit_held` fires.

Eligibility declarations, orders, withdrawals and custody credits depend on a signed screening
answer from the access source, which `[access]` configures. When the source cannot be reached, or
its answer is stale, the affected action is refused and nothing else changes:

- `access_source_failed` (Warning): the source refused or failed, and the request stays unexecuted.
- `access_deposit_retry_failed` (Warning): pending deposits stay uncredited, because the retry
  cannot screen them.
- `access_deposit_held` (Critical): a screening decision holds a deposit, and only the source's
  later decision can release it.

1. Restore the source, or its credentials. The source's URL is in the configuration, so changing it
   is a new release. The source's signing authority is recorded in the log, and replacing it needs a
   fresh log.

**How to tell it is resolved.** The alerts resolve, and pending deposits are credited.

## How a price exists

Each market needs at least `min_sources` quotes, which is 2 for every launch market except the
seven futures markets described below, that are no older than its `max_age_ms`, which is 1 second, and that agree to within its `max_divergence_bps`.
Otherwise the market has no price. No order in it can be executed, including an order that closes a
position, and positions in it that fall below the close-out level wait until the market has a price
again.

There is one exception, for a market with a weekend margin, and the production settings give no
market one. When such a market starts its weekend gap, the close-out executes at the market's last
mark, which is the last price before the market closed, however old that price is.

A market priced from two sources has no price when either source fails, or goes quiet for longer
than `max_age_ms`. That holds for the crypto markets and for the markets that data vendors price.
The seven futures markets (20 to 22, 25 and 30 to 32) take `min_sources = 1` and price from Massive's
futures feed alone, so they have no price while Massive's futures connection is down or quiet.
