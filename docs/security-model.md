# Security model

This page states what each component of monkeytrading guarantees, and then its known limits. The
[security policy](../SECURITY.md) explains how to report a vulnerability, what is in scope, and when
a report counts as a new finding. A report that rests only on a limit stated here is not a new
finding.

- **Trading:** [signed requests and answers](#signed-requests-and-answers),
  [account screening](#account-screening),
  [trading rules and the operator's bounds](#trading-rules-and-the-operators-bounds),
  [price feeds](#price-feeds)
- **The log and custody:** [the published log](#the-published-log), [deposits](#deposits),
  [withdrawals and the exit](#withdrawals-and-the-exit),
  [the vault's governance](#the-vaults-governance),
  [MONKEY staking and distributions](#monkey-staking-and-distributions)
- **The enclave:** [the enclave and its keys](#the-enclave-and-its-keys),
  [the enclave's clock](#the-enclaves-clock), [public certificates](#public-certificates)
- **Operator rotation:** [elections and daily turns](#elections-and-daily-turns),
  [the seed relay](#the-seed-relay),
  [the log check before withdrawals](#the-log-check-before-withdrawals),
  [liveness and promotion](#liveness-and-promotion), [fault proofs](#fault-proofs),
  [parameter votes](#parameter-votes)
- [Development settings](#development-settings)

The operator runs the trading server, and signs its administrative instructions with the operator's
key. A node, in commands, configuration keys and output, is a running trading server.

A log written under an earlier protocol version replays under that version's rules. Where a limit
applies only to such a log, it says so, and the
[version list](../crates/core/src/protocol/versions.rs) describes each version.

## Signed requests and answers

Every request that acts for an account, such as an order, a cancellation or a withdrawal, must carry
the account's EIP-712 signature, or the signature of a session key that the account registered. The
engine accepts no contract signatures. A contract wallet therefore trades through a key that it
funds with the vault's `depositFor`.

The enclave signs every API response that a client relies on with its attested key. The signature
covers the request, the response, and a challenge that the client chose. A client that sends a fresh
challenge can therefore tell a forged, replayed or substituted response from a genuine one. The
enclave signs only while its trusted time, which it takes from public Roughtime servers, bounds the
host's clock. On a stream connection opened with `signed=1` and a challenge, the enclave numbers and
chains every frame, so the client can also tell when a frame was dropped, repeated or taken from
another connection.

Known limits:

- Anyone between a client and the trading server can delay or drop traffic, and signatures cannot
  prevent it. The stream's signed heartbeats let a client detect it.
- A signature binds a response to the bytes of the request and challenge it answers, not to one
  delivery of them. Someone in between can send the trading server a copy of a request, or open a
  copy of a stream connection, and the trading server answers each copy. A refusal can therefore
  answer a second copy of a request whose first copy was executed.
- Before a client shows a refusal of an order, a cancellation, a withdrawal or any other request
  that changes state, it must confirm the refusal with a signed read of the account under a fresh
  challenge. The account's next nonce does not show whether the request was executed, because a
  request refused for its expiry also uses its nonce.
- A response to a request sent without a challenge can be replayed as the response to another such
  request.
- These responses are unsigned: a browser's preflight; a refusal by the origin check, the rate
  limiter or the body read; a refusal of a malformed `X-Engine-Challenge`; every response while
  trusted time does not bound the host's clock; and every response while the operator's enclave is
  fenced before a possible [promotion](#liveness-and-promotion).
- While responses are unsigned, a signed stream connection closes, unless it carries
  [log heads](#the-published-log), in which case it sends the heads without frames.
- The attestation, its collateral, the public archive and the health check are unsigned, because
  they prove themselves or carry nothing to rely on.
- The time that a response carries is the host's clock. It is signed only while it lies within
  `time.max_drift` of trusted time, which is 15 seconds in production, so it is accurate to that
  bound.
- A contract account cannot sign a request, so it cannot trade, withdraw or declare its eligibility.
  A deposit made for a contract account is therefore never credited. It stays pending, and the
  [exit](#withdrawals-and-the-exit) refunds it to its payer. A report of this alone is not a new
  finding.
- A log written before requests were signed replays its unsigned requests. A log written before
  account screening keeps the authorisation rules it recorded. In a log from before eligibility
  declarations, a balance credited to a contract account can be recovered only through the exit.

## Account screening

Before an account's first deposit is credited, or its first order is accepted, the account must sign
an eligibility declaration. Declarations, orders and withdrawals each require fresh screening of the
account, and of the client's network address, by the screening source that the log records. The
source's evidence must be signed by the authority the log records, and it is bound by its freshness,
its subject and the request it answers. A deposit from a payer confirmed as sanctioned is held
uncredited, and the trading server publishes the evidence for the hold.

Known limits:

- A deposit for an account that has not declared stays pending and uncredited until the account
  declares. Meanwhile the trading server re-screens it only for a confirmed sanction.
- When the source returns an unknown classification or fails, the deposit stays pending and
  uncredited, and the trading server retries it automatically. The retries are bounded in rate, not
  in number.
- A wallet can declare and manage its session keys before its first deposit. What a wallet with no
  funds can add to the public log and to the state is therefore bounded by the fresh-account rate
  budgets, not by the cost of a deposit.
- Each client address holds a share of the shared fresh-account budget that is smaller than the
  whole. A client with several addresses can still exhaust the shared budget, and new wallets are
  then refused until it refills. A wallet with a pending deposit declares outside those budgets,
  against a budget of its own.
- The API's screening slots are shared in the same way. Each account and each client address holds
  at most its share, and closing positions and withdrawing keep a reserve that orders opening
  positions, and declarations, cannot take. A client with enough addresses can still hold every slot
  while the source is answering.
- A hold for a confirmed sanction has no automatic release or refund.
- Jurisdiction screening uses the address of the connection's peer or, behind a configured
  pass-through proxy, the client address in the proxy's PROXY protocol header. The host's network
  supplies both addresses, so neither is bounded against a host that relays or translates client
  connections.
- The engine checks the source's signature, freshness and subject, but it trusts the source's
  classifications. Their correctness rests on the source's provenance, which no check in the code
  establishes.
- The local screening source fixture is synthetic. A trading server with real attestation refuses
  its published signer.

## Trading rules and the operator's bounds

The engine applies the trading rules as code, with the house, the engine's own account, taking the
other side of every trade. The operator's key can move a trading setting only within the bounds
below, which the engine enforces. While [parameter votes](#parameter-votes) govern, a vote takes the
place of a notice, within the same bounds, and a fast-path vote can tighten some settings at once.
Sequenced time, used below, is the time that the trading server stamps on each instruction it
processes. The bounds are:

- No spread band is below its asset class's floor.
- A listed market's spread and margin bands change only after seven days' notice. Each spread rate
  stays between its launch rate and twice that rate, and each margin rate stays at or above its
  launch rate.
- The spread charged on reducing or closing a position never exceeds the market's closing spread
  cap. A cap is installed or adopted only at or above what closing a position in the first band pays
  at the widest stress level allowed, under both the parameters in force and those the market
  launched with. After listing, a cap can only be lowered.
- The close-out level moves only after seven days' notice, and only within 50% either side of its
  launch value.
- No holding time pays a lower profit fee than at launch. The base fee moves only between its launch
  value and one and a half times that value. The fee curve moves only to one that is at or above the
  launch curve at every holding time, and that reaches its base rate within 30 minutes at most. Each
  change takes effect after seven days' notice.
- When you add to a position, its opening time becomes a size-weighted average, in which the part
  already held counts as no older than the last holding time of the curve the log launched with.
  This holds whatever curve is in force.
- The volume tier table changes only after seven days' notice, and it keeps its launch tiers. No
  volume threshold or day divisor falls below its launch value, and no best-price share rises above
  its launch value. At every tier and position size, each market's spread stays between that tier's
  launch spread and twice it, as does the spread of every tier below one whose threshold or day
  divisor was raised.
- Each tier's margin floor stays at or above its launch value, and a higher tier's floor is never
  above a lower tier's. The initial requirement, which new positions and withdrawals must meet,
  charges every margin band at least the account's tier floor. Close-outs read only the market's own
  bands. A trade or withdrawal admitted below that requirement, or a close-out caused by a tier or a
  floor, is in scope.
- Each tier's limit on working orders, and its request rate, is set for every tier or for none.
  Where the launch values set them, as the production values do, each stays at or below its launch
  value, and neither falls as the tier rises. A new working order beyond the account's tier limit is
  refused, while orders already working stay. The trading server admits a credited account's signed
  requests at its tier's rate. A limit raised above launch's, or an order placed beyond a tier's
  limit, is in scope.
- Each tier's fee discount, financing discount and emissions boost stays at or below its launch
  value, and none falls as the tier rises. No notice or vote can therefore make a higher tier pay
  more fee than a lower one, or raise a benefit above its launch value. A fee or financing charge
  below what the tier's launch discount allows is in scope.
- The daily rollover time moves only after seven days' notice. A new rollover time keeps the
  rollover already due. Each rollover charges one day's financing for each day of sequenced time
  since the previous rollover, and financing charged faster than this is in scope.
- A listed market's price rules are loosened only after seven days' notice, and never beyond the
  market's launch rules. Tighter price rules apply at once. Price rules loosened beyond these bounds
  are in scope.
- A market's minimum distance for take profit and stop loss levels moves only after seven days'
  notice, and only between its launch value and twice that value.
- A dividend or split stays within the corporate bounds in the parameters. Those bounds move only
  after seven days' notice, and never beyond their launch values. While votes govern, only a vote
  can schedule a dividend or split. At launch, the bounds are:
  - the cash dividends that a market pays with less than seven days' notice total at most 10% of the
    share's reference price within any seven days, measured in the units held at their capture;
  - a split is at most 20 to 1 in either direction, needs seven days' notice, and applies only after
    the capture of every pending dividend;
  - at least two of the market's trading sessions close, on distinct dates, between the action's
    scheduling and its previous close. That previous close falls at least one day after scheduling
    for each trading day beyond the first.

  A dividend scheduled by a vote is not held to the 10% limit, and a split scheduled by a vote does
  not need seven days' notice. The ratio limit, the trading-day rule and the order after pending
  dividends still apply to both. A corporate action already pending when these bounds first applied
  to a log keeps the rules it was scheduled under. A corporate action applied outside these bounds
  is in scope.
- The production settings give no market a weekend margin, so margins do not change with the time
  of day or week. A weekend margin is a higher margin over a market's weekend closures, which are
  closures of at least 40 hours. One can be added to a listed market, or changed, only after seven
  days' notice.
- For a market with a weekend margin, a calendar change by the operator cannot narrow or remove a
  weekend closure that starts within seven days. While such a market holds positions, a calendar
  change by the operator also cannot add a weekend closure, or bring one forward, within seven days.
- A weekend margin applies to new positions and withdrawals from two hours before a weekend closure
  until the market reopens. An account below it when the closure starts is closed out in that market
  at the market's last price. A weekend margin change beyond these bounds, or a position held across
  a closure below its weekend margin, is in scope.
- The house loss is the house's net asset value less the net capital moved into it. Once the house
  has lost money to its clients over the trailing day, every market rises to the stress level that
  the house loss trigger sets. The stress levels keep that trigger at its launch level or above,
  with a threshold no higher than its launch value. A trade or close-out executed below that level
  while the house is behind on the day is in scope, and so is a change beyond this bound.

Known limits:

- The house loss trigger caps how fast the house loses, not what a fast client takes from the
  house's income. [The loss-rate trigger study](../research/loss-rate-trigger/README.md) gives the
  bound and the kinds of trading it leaves open.
- Margins stay the same across a market's closures, and no market carries a weekend margin. The
  house therefore bears any loss beyond an account's equity when a market reopens at a different
  price. The exposure caps bound the house's net position in each market, but not that loss.
- A pair of opposite positions in two accounts leaves the house's net position unchanged, so no cap
  limits it. Across a reopening, one account keeps its gain while the house bears the other's loss
  beyond its equity. Held across every weekend, such pairs lose money on average at volume tier 0's
  limit of 20 times leverage on every market, and at tier 1's limit of 50 times they pay only on WTI.
- At a market's own leverage, from volume tier 2, pairs held across every weekend pay on average on
  WTI, Japan 225, silver and US Tech 100, and the take grows with the margin posted, as
  [the launch-caps study](../research/launch-caps/README.md#paired-positions-across-session-gaps)
  measures for each market. A single gap wider than a position's margin and spreads pays a pair at
  any tier.
- A host that delays or drops a market's quotes lowers that market's markout spread, the unit of the
  toxicity score, which inflates every account's score in the market. The markout floor that each
  listing names bounds this, and the floor cannot change once the market is listed. Each launch
  floor is the market's markout spread over a year.
- A position opened and closed at the same reference price within the five-second horizon adds
  nothing to the toxicity score on either trade, while one held across a price move counts on both.
  Diluting a score therefore takes trades that bear the price's moves over the horizon.
- Such a position also counts nothing towards a volume tier, and volume counts only once a new
  position has been held for 60 seconds of sequenced time. A host that delays a client's order to
  open a position delays when its volume counts, and one that holds back a closing order holds back
  that count. Neither grants volume that was not traded.
- A host trading through its own accounts earns a tier only by holding the volume and paying both
  spreads on it, and the profit fee on any gain.

## Price feeds

Only a venue reached over TLS that ends inside the enclave can set or move a price. Every venue
endpoint is a `wss://` address on the venue's own domain, and its certificate is checked against
root certificates compiled into the binary. The host can therefore delay or drop a venue's quotes,
but it cannot alter them.

A production build refuses a configuration in which fewer venues quote a launch market, or any
futures contract that its listing names, than the market's `min_sources`. Every production market
needs fresh quotes from two venues, so a delay beyond the bounds below on either venue leaves the
market without a price. Two connections to one venue, such as Coinbase's Advanced Trade and Exchange
feeds or Kraken's two connections, carry one source name. The engine holds one quote per source, the
later-stamped, so a venue read twice still counts once, and either connection keeps the venue's part
of the price while the other fails.

The host carries every byte to and from the enclave, and it sets the wall clock. A host could hold
back a venue's stream and set its clock back to match, so that the venue's timestamps would still
look fresh. The drift bound on the wall clock alone would then let prices run up to `time.max_drift`
plus `max_age_ms` behind the market. The age of a price is therefore bounded on the enclave's
monotonic clock instead:

- Each venue connection sends a ping every `feeds.venues.ping_interval`. A TLS stream is ordered, so
  the venue's reply to a ping arrives only after every frame the venue sent before it. Any frame
  read after that reply was therefore sent after the ping.
- A quote is processed only within `feeds.venues.max_delay` of the newest ping that had been
  answered when the quote was read. This holds whether the host held the quote back on the network,
  or held back the trading server by stalling its disk or its processor.
- Sequenced time keeps pace with monotonic time whatever the host clock does, falling behind by at
  most 10 milliseconds per second. After a restart, the trading server stamps nothing and drops
  venue quotes until that pace, counted from the last stamp, has passed every quote carried across
  the restart. For a market whose `max_age_ms` is longer than `time.max_drift`, this holds only as
  far as trusted time admits. A quote therefore counts for at most about its market's `max_age_ms`
  once it reaches the engine.
- A stall after an instruction is stamped delays that instruction, but not the stamp of any
  instruction after it.

A quote that sets the price at which an order is executed was therefore sent by its venue at most
about `max_delay` plus `max_age_ms` earlier. Under the production values of 2 seconds and 1 second,
that is 3 seconds, whatever the host clock reads and however the host delays the network. A book
that a venue has stopped updating can be priced for longer, as the limits below describe.

Known limits:

- Certificate validity is judged by the host clock, and revocation is not checked. A host holding a
  venue's private key that has expired or been revoked could therefore pose as that venue.
- A market listed after launch is not held to its `min_sources` by the build. Only the
  `market_unfed`, `market_no_price` and `futures_roll_overdue` alerts watch it.
- A venue sends a book only when the book changes. On a Coinbase, Kraken or Bitstamp connection, a
  ping reply with no change to a symbol's book since a read before the ping went out processes that
  book's mid price again, which re-dates the book. The re-dated book is stamped no later than the
  venue's clock, or the host clock, when the ping went out, and is held to that ping's deadline. It
  therefore keeps the same bound, and a host that delays the reply only ages it.
- Pings re-date a book for at most 5 seconds after the newest ping that had been answered when the
  book was read. A venue whose book stream stalls while its connection still answers pings can
  therefore hold its part of the reference price for 5 seconds plus `max_delay` plus `max_age_ms`
  after it sent the stalled book. Under the production values that is 8 seconds, or about 6 seconds
  when nothing delays the stream.
- Meanwhile the market keeps a price only while its venues' quotes lie within `max_divergence_bps`
  of their median, counted in whole basis points. For BTC-USD, ETH-USD and SOL-USD, each priced by
  two venues with a 25 bps limit, a stalled venue moves the reference price at most about 13 bps
  from the venue still quoting. That is half the limit, plus the rounding to whole basis points.
  Beyond it, the market has no price.
- Re-dating rests on these venues writing a ping reply behind everything they sent before reading
  the ping, as they were measured to do. The Massive, TraderMade, CME Group and Alpaca connections
  were not measured and re-date nothing, and neither does the Coinbase Exchange connection.
- The ping bound holds against the host, which cannot make a ping reply overtake what the venue sent
  before it. It does not hold against a venue that lags upstream of its own send queue. Coinbase has
  served one product's book from a source minutes behind its own, while answering pings at once and
  keeping the connection's other products current.
- Prices stay safe in that case because each quote carries the venue's own time. The engine counts
  no quote older than `max_age_ms`, and re-dating moves a book's stamp on only by the time since the
  book was read, so the market loses that venue rather than taking its old price. A book sent after
  a re-dated one takes the re-dated book's stamp only when its own stamp would still count.
- Books that arrive already older than they count for, measured from when the venue sent them, for 2
  seconds raise `venue_stale` and resubscribe the product. The product is resubscribed alone on its
  connection where the venue allows that, and on a new connection if it stays stale, again after
  each further 2 seconds of such books, with backoff.
- Every part of this bound rests on the rate of the monotonic clock, as
  [the enclave's clock](#the-enclaves-clock) describes.

## The published log

The trading server sequences instructions, which means that it orders them, stamps them and applies
them to the engine. It publishes every instruction, together with the events that the instruction
produced. At regular intervals, called checkpoints, the enclave signs a root over the entire state
and posts it to the vault. Anyone can replay the log with this repository's code, and check the
signed root at each checkpoint. Replies, stream entries and signed withdrawals leave the trading
server only once the batch that holds them is durable on disk.

With `[checkpoints]` configured, the trading server verifies its replayed log at startup against the
latest commitment posted to the vault, at custody finality. It refuses a shorter or conflicting log
before it serves anything or opens its history for writing. Checkpoint signatures are verified
against their chain, their vault and their named signer. Each new checkpoint also signs the exact
sequence number and root of its predecessor, which the vault compares atomically, so two publishers
with one key cannot both extend the vault's chain.

With `[log_heads]` configured, the enclave also signs log heads. A log head states the sequence
number of the newest instruction the trading server has released, and a running digest of every
instruction up to it. Checkpoints do not reset the digest, so a posted head commits to the whole
history. A dedicated relayer posts each head to the `LogHeads` contract. The contract checks the
head's signer against the vault's `checkpointSigner` and `signerEpoch`, and keeps the highest
sequence number for each signer epoch, which is the period during which one signer is installed in
the vault. Each post names the previously accepted head it extends, so a longer divergent copy
cannot replace the anchor without containing it.

Before a restarted trading server writes anything, it reads the posted head through the custody RPC
quorum. It refuses to start on a log that lacks that head, and raises the Critical alert
`log_rollback`, naming the missing range or the sequence number whose digest differs. A posted head
that overtakes the running trading server, at a higher sequence number or with another digest at a
sequence number it retains, freezes sequencing.

Known limits:

- These checks reach only as far as the vault's checkpoints and the posted heads. Within that reach,
  whoever runs the trading server can choose after the fact whether its own recent instructions
  happened, by holding a sync or by cutting the log back, as the items below describe.
- **Holding a sync.** The host can hold a batch's sync to disk while it watches the market, then let
  the sync finish or kill the trading server. Until the batch is durable, nobody outside has seen
  it, and a batch that the restarted log lacks never happened.
- Once the oldest unsynced batch has waited 5 seconds, the trading server accepts no further request
  and raises the Critical alert `log_sync_held`. Nothing is stamped behind a held batch, so a hold
  of the log's disk alone is raised within 5 seconds.
- The alert's webhook page is first written to the alert outbox on the same disk, so a hold of the
  whole disk delays it, and a kill during the hold loses it. Meanwhile the `engine_log_sync_held`
  gauge, served from memory on the metrics listener, reports the hold (`EngineLogSyncHeld`),
  although a host that holds the disk can also answer for that listener.
- The snapshot capture's sync, the checkpoint's catalog maintenance, a rotation's catalog fold and a
  handover's segment seal are written on the thread that processes instructions, outside the
  5-second bound. A hold there also stops sequencing, and raises `sequencer_stalled`.
- What a hold can gain the host is bounded as its ordinary trading is. Its instructions are still
  priced at their stamps under the size, stress and toxicity rules, and the house's exposure caps
  still limit what it can open.
- A hold lasts only as long as the host stalls every reply and every new stream entry, which every
  client sees. The signed heartbeats keep naming the old head unless the host also holds the
  network.
- Posting log heads does not close this gap, because nothing anchors a batch before it is released.
  A host that kills the trading server before a post can withhold that post too.
- **Cutting the log back.** The host can cut the log back and restart the trading server, erasing
  instructions that it had already answered and streamed. With `[log_heads]`, a cut goes undetected
  only if it removes nothing at or below the latest posted head.
- Heads are posted at most once per `log_heads.interval`, which is an hour by default, and only when
  the head has moved. While a turn names the operator, heads are posted at least twice per heartbeat
  interval instead, as [liveness and promotion](#liveness-and-promotion) describes. To pass
  unnoticed, a cut must land within about one posting interval, plus the time to include a post on
  chain, of the log's end. A host that stalls or withholds the poster's submissions extends that
  window by however long the tail stays unposted.
- The running trading server adopts an on-chain head as the parent of its next post only when the
  head's digest is the one its own log holds at that sequence number, since a second copy of the key
  signs correctly too. A head at a sequence number for which it retains no digest is not adopted, so
  its posts fail (`log_head_post_failed`) and the head stalls (`log_head_stalled`) until a restart
  checks the posted head against the whole log.
- `log_head_stalled` is raised once the head has not advanced for three posting intervals, or for
  two with `operator.rotation_enabled`. The count runs in trusted time from a stamp that recovery
  authenticates, which is the stamp of the head that startup checked or, while no signer epoch of
  this key holds a head, the stamp of the checkpoint the vault has finalized. The count starts once
  the restarted poster's first post has had a posting interval to be included.
- Neither cutting the log back nor restarting less often than once per posting interval therefore
  keeps the alarm quiet. A host that restarts the trading server more often, with each stop shorter
  than the minute after which the alert for a stopped trading server fires, does keep
  `log_head_stalled` quiet. Blocked submissions still raise `log_head_post_failed`, and every start
  is logged.
- While no signer epoch of this key holds a head, the bound on a cut is the vault-finalized
  checkpoint's, as without `[log_heads]`. A restart then counts from the newest such checkpoint.
  Restarts spaced more closely than the number of posting intervals that the alarm counts therefore
  keep it quiet while checkpoints finalize.
- Without `[log_heads]`, a cut goes undetected only if it removes nothing at or below the latest
  checkpoint the vault has finalized. A cut below a later checkpoint that the vault accepted but has
  not yet finalized shows when the checkpoint poster next compares a checkpoint with the vault. The
  first such conflict raises the Warning `checkpoint_post_failed`, and a repeat raises the Critical
  `checkpoint_conflict`.
- A cut after the latest checkpoint the vault accepted goes unnoticed by the trading server.
  Checkpoints go out about once per finality delay of the custody chain, so about two finality
  delays of instructions can be erased while checkpoint delivery is healthy. A host that stalls or
  withholds the poster's submissions extends the window by however long the tail stays unanchored.
- Clients keep the signed replies and frames they received, but the trading server does not compare
  them with the restarted log.
- **A standby's check.** A standby, a trading server that follows the operator's signed stream into
  a copy of the log, does compare. It takes an instruction only when its seal continues the copy and
  is the followed enclave's, or one that a checkpoint in the copy names, so history sealed before a
  handover still extends it. Each stream it resumes must first repeat the copy's last instruction
  exactly.
- When the operator's log no longer holds what the standby copied, the standby raises the Critical
  alert `operator_log_rollback` through its own alert routing, naming the range, and keeps its copy
  unchanged to restore from. The signs are a signed heartbeat behind the copy, a repeated
  instruction that differs, or an instruction that does not chain. The standby's check reaches only
  as far as what it received, so it detects only a cut that removes instructions it had received.
- **Signer changes.** Posted heads are kept per signer epoch, and a new signer epoch starts with no
  posted head. A standby's takeover, which drops the outgoing key's tail by design, is therefore not
  refused.
- A key that `setSigner` reinstalls, as after the guardian retires it, also starts a new, empty
  epoch. While that epoch holds no head, startup walks back through the earlier epochs the key
  signed and checks the newest head any of them holds, all at one block that the custody quorum
  agrees on. The reinstalled key's own earlier head therefore still binds its log.
- A promoted member checks its log against the vault at the latest block, not at finality, because
  it starts before its promotion is final. It accepts the vault's latest checkpoint when that is the
  checkpoint its promotion fixed, or one its own key signed.
- The trading server trusts the RPC endpoint that carries its checkpoints for the vault's state, and
  the deposit quorum does not protect that endpoint. A checkpoint signature's named signer must be
  checked against a trusted attestation or the vault's authorized signer.
- **Snapshots and local stores.** Optional recovery snapshots require attested signer pins
  provisioned independently, and they authenticate the complete snapshot, its deployment and its
  checkpoint. With snapshots, account history and the segment catalog are read and updated only
  through proofs against roots the trading server signed. Their signatures establish integrity, not
  freshness, so the same independent check against custody applies, and a full `engine replay`
  checks the log segments a snapshot skipped.
- The alert outbox keeps pages on the host's disk. It checks their schema and rows, but cannot
  detect their deletion or rollback, and its age budget across restarts uses the host clock.
- Without snapshots, the account history index is read from the host's disk with only its schema and
  integrity checked, and its rows are not authenticated. It is matched to the log by one
  instruction, the newest it has taken. SQLite reads it in defensive mode, and each event it serves
  names its place in the log, where anyone can check it.

## Deposits

The trading server credits a vault deposit only when a strict majority of at least three independent
RPC providers, as configured, agree on the deposit, the block hashes and finality. Reads are
compared at the same block height. An empty answer for a range of blocks counts only if the
answering provider serves the range's final block. A provider excluded by a poll's identity vote
cannot vote again, and exclusion never lowers the configured majority, so a provider on the wrong
chain paired with a malicious provider that mirrors its answers is rejected. A dissenting or
unavailable provider raises an alert. A lost quorum raises a Critical alert and pauses crediting,
while trading continues.

A deposit can be traded once Arbitrum's sequencer has confirmed it, which is the `latest` block less
the configured confirmations. Its principal can be withdrawn only after Ethereum finality, which is
the `finalized` tag. A trading server with real attestation refuses any other pair of tags. Exact
receipts, unfinalised principal and finalised scan progress are deterministic state in the log, so
the host's scan cursor cannot move them.

When the quorum proves that a deposit was reversed before finality, the engine debits it once,
raises a Critical alert, and runs the usual close-out, writing off against the house any loss on an
account left with no positions. Positions without a price wait for one, and a negative house net
asset value does not halt trading. A deposit included again in another block, with the same
beneficiary and amount, moves to its replacement receipt in one step, even at a changed log index.
Its cash nets out, no close-out runs, and a Critical alert is still raised. An orphaned deposit with
no replacement up to the eligible head is debited.

Known limits:

- The quorum's guarantee assumes that an honest majority of providers serves the intended chain and
  token, which endpoint independence alone cannot establish. A compromised majority is trusted, and
  the independence of the providers' operators cannot be verified by machine.
- The vault, its token, the provider endpoints and the finality tag come from the host-supplied
  configuration. Only a verifier holding a published copy of that configuration can check it against
  the attested configuration hash.
- Reversals before finality, and the handling of a missing quorum, are in scope.
- In a log from before deposit finality, a host cursor decides which blocks are scanned, and
  advancing it can skip deposits. Checkpoint reconciliation detects the resulting mismatch in the
  deposit count, and escalates after its configured catch-up deadline.
- When such a log activates deposit finality, its EVM credits gain no finality from their old tag,
  their credit status, the host cursor or a change of configuration. Activation records their
  verified credit ledger and locks unresolved principal without crediting cash again. An exact proof
  of the canonical receipt, or withdrawal finality, releases each lock independently.
- Missing or ambiguous historical identity keeps that principal locked, with a Critical alert, and
  an adopted receipt takes the normal reversal path. Before it opens the log for writing, recovery
  refuses to sign a stranded legacy approval that consumes unresolved principal until an independent
  proof of finality succeeds.
- Inherited unsigned approvals keep their authenticated approval prefix across activation, and later
  funding needs checkpoint coverage of its own sequence before its signature is released. A proof of
  finality authorizes only the inherited obligations it proves safe, and never applies cash again.

## Withdrawals and the exit

The vault pays a withdrawal only when all of these hold:

- the current signer authorized it, in the current signer epoch;
- its account nonce has not been used;
- it is presented within 24 hours of signing;
- a proof shows that its approval is included in an accepted root.

A withdrawal signature never settles its approval. Finalized evidence that the custody nonce was
used settles it, and replay clears an approval on that evidence only with an independently trusted
signer and deployment.

An expired authorization is replaced only on its account's signed request, which access screening
checks like a new withdrawal. It is never replaced by a signer rotation, on a schedule or on
restart. A rotation replaces only an authorization that is still usable, and the vault rejects
retired epochs at once. The history of accepted checkpoints authenticates historical roots, and the
immutable history of signer epochs independently authenticates reissues against older roots during
recovery and replay against custody. The tail of checkpoints that the vault has not yet anchored
still requires the current signer.

Startup and the release of signed withdrawals use custody's withdrawal finality tag, independently
of the credit tag and its confirmation depth. Inclusion in the latest block alone never authorizes a
release. New withdrawals pause while the latest finalized checkpoint's custody prefix is unproven
against canonical receipts, and resume once a checkpoint with proven coverage reconciles. A failed
chain read never lifts a pause or erases a proved divergence. A trading server with real attestation
and EVM custody requires `[checkpoints]`, which provides this protection.

If no checkpoint has been posted for the exit period, which is 14 days at launch, anyone can start
the permanent exit against the last posted root. The exit ends signed withdrawals, checkpoints and
distribution claims, and registers claims proven against that root during the claim window. It adds
a deposit's unfinalised principal to a claim only where the vault's own record of the deposit
matches the credit that the root commits for it, with the same payer, amount and beneficiary. The
exit refunds to its payer each deposit that the root holds as pending, or does not hold. It also
refunds the deposit at any id whose credited, pending or held receipt in the root does not match the
vault's record, so a receipt that a reorganisation removed is not paid from other clients' funds.

Known limits:

- While operator rotation runs, the enclave also signs a withdrawal only once a check by another
  member covers its approval, as
  [the log check before withdrawals](#the-log-check-before-withdrawals) describes.

## The vault's governance

The deployment's multisig proposes changes through a timelock with a nonzero delay, which is three
days at launch. The timelock lifts a withdrawal pause, changes the deposit pause, and names the
release that may hold the enclave's keys. While operator rotation is off, it sets one launch
measurement and configuration hash as the current release. While rotation is on, it approves
releases for the operator in effect, whose candidate account moves the vault's current release to
one of them, withdrawing the approval of the release it leaves.

The timelock installs the vault's signer. While rotation is on, it can install only the enclave
address that the operator in effect registered. The vault also moves its signer, with no timelock,
to a member's vouched standby key when the schedule promotes that member, as
[liveness and promotion](#liveness-and-promotion) describes.

The vault's guardian is fixed at deployment, and the deployment sets it to the same multisig, which
acts as guardian without the delay. The guardian may pause withdrawals for seven days, and it cannot
extend that pause. It may also retire the signer. After a retirement, the vault refuses withdrawals,
checkpoints and promotions until the timelock installs a signer. It refuses MONKEY and USDC claims
until that signer posts a checkpoint, which an exit that starts first rules out.

Known limits:

- The vault trusts the timelock's choice of signer. While rotation is off, the timelock can install
  any signer. While rotation is on, it can install the enclave address that the operator in effect
  registered, which the chain does not check against an attestation.
- The trading server learns of a pause or a retirement from finalized evidence. For up to one
  finality delay after either, it can still sign authorizations, which the vault refuses.

## MONKEY staking and distributions

The vault pays each account its MONKEY emissions, and each staker its USDC distributions, against
entitlements proven against the root of any recorded checkpoint of the signer epoch in force, less
what the account has already claimed. The staking contract holds staked MONKEY, enforces the unstake
cooldown, and emits the events that the engine mirrors from finalised blocks. The staking mirror's
references to those events, and the anchors that record how far it has scanned, are part of the
state root. The mirror's frontier is the latest finalised block it has mirrored. A distribution
epoch closes only once that frontier has passed the epoch's end, so no epoch's close pays a stake
set that the mirror has not brought up to the epoch's end.

Known limits:

- The staking contract keeps each staker's delegations as amounts, with no limit on how many
  candidates one staker delegates to. An unstake larger than the staker's undelegated stake reduces
  every delegation in one transaction, so a staker with very many delegations must first take some
  back, one or two per call, before such an unstake fits in a block.
- That cost falls only on the staker that chose the delegations, and no one else can add to them. A
  report that rests on it alone is not a new finding.
- The host can delay a distribution by holding back the mirror, and an epoch that it holds back for
  a day past its end is skipped.
- The operator's share of each distribution is split among operators by their sequenced time in
  office. An incumbent whose host delays an elected successor's takeover therefore keeps earning the
  share through that delay.
- In a log from before staking mirror references entered the state root, recovery that can write to
  the log needs authenticated origin seals on the older staking instructions.

## The enclave and its keys

The trading server runs inside an SEV-SNP enclave built from a reproducible image. Its attestation
report binds the enclave's signing address, the SHA-256 hash of its configuration file and the hash
of its TLS key. `engine verify-attestation`, and the browser build of the same code, check a
report's certificate chain, measurement, policy, firmware versions and configuration against a
release manifest.

The enclave's keys are sealed on the host's disk under a key that the SEV-SNP firmware derives from
the chip, the launch measurement, the guest policy and the committed firmware versions, with the
configuration's hash bound to the ciphertext. Neither the host nor another release can open them,
including the same code under another configuration.

A trading server opens or seals its keys only while the vault names its release as the current
release, and only in the vault's current generation of that release, which every change of the
current release raises. It reads the current release at custody finality, except from a daily turn's
boundary, when it reads the turn that the finalised schedule names at trusted time. Keys are sealed
or handed over only once that generation has taken the change into account. A release that the
timelock has superseded therefore never signs again from its old sealed file, even if it is named
again later.

The keys move to a new release only after the vault names it, and only after each release attests
the other over a challenge fresh to the connection. They travel encrypted to the new release's
attested one-time key, and the new release takes them only if the vault already names their signing
key. A release that has stopped in order to hand over its keys serves new releases until one
confirms. A trading server that the vault does not name as signer signs nothing and serves only its
attestation.

The trading server's credentials reach it only encrypted to a one-time key whose quote proves that
an enclave of the release holds it. They are its RPC provider keys, its market-data vendor keys and
OAuth client credentials, its screening source key, the seed relayer's Ethereum provider and beacon
node URLs, and its alert webhook URLs. The operator's tool checks the quote against the release
manifest before it encrypts the bundle, and the bundle must carry the signature of the operator key
in effect. The attested configuration names which secret each field reads, and fixes each RPC
provider's origin, so a bundle, even one signed by a stolen operator key, can change a provider's
key but never which provider credits deposits. Secrets are sealed per release, a recorded delivery
does not decrypt on a later connection, and an election moves none of them.

Known limits:

- The SEV-SNP sealing key, the hardware quotes exchanged in a handover and the image's Secure TSC
  launch are built to the firmware's guest interface, but have not yet run on SEV-SNP hardware.
- With the mock enclave, the stand-in for the sealing key is derived from public values, so anyone
  can compute it.
- Two copies of the current release that the host runs from copies of one disk can both open the
  keys. The vault accepts one copy's checkpoint and the other copy stops on the conflict, but until
  it stops, its signed API answers still verify.
- A release taking over the keys checks the running release's certificates against the host's clock.
  The vault's naming of the key bounds this.
- A superseded release refuses its own sealed copy only by reading the vault's current release
  through the checkpoint RPC endpoint, which the trading server trusts for the vault's state. A
  superseded release that is still running keeps signing until it hands over its keys, and raises
  `release_superseded`. A flaw in a release that has already run still needs a signer rotation.
- A holder admits a skip's member, and hands it the keys, from the latest block that the custody
  quorum agrees on, before the skip is final, as [liveness and promotion](#liveness-and-promotion)
  describes.
- A host that restores an older sealed file of delivered secrets gets the older bundle, whose
  version the enclave's attested status shows. Nothing bounds such a restore more tightly, so the
  operator revokes a rotated credential at its provider.
- Until the log is recovered, the operator key in effect for a delivery is the one the attested
  configuration names, which can be a key the log has since rotated away from. Once the trading
  server serves, it is the key the log records. While the registry elects operators, each trading
  server takes bundles only from its own configured key.
- The image's kernel takes input from the host that the launch measurement does not cover, and does
  not yet bound it: the data disk's filesystem, the ACPI tables, the emulated devices and the
  network configuration.
- The trading server relays the vendor's certificate chain and revocation list for verifiers that
  cannot reach the vendor. A verifier pins the vendor's root certificates itself, but a host that
  blocks the trading server's fetches delays a revocation until the served list's `nextUpdate`.

## The enclave's clock

Every bound on time rests on the rate of the enclave's monotonic clock, as do the Roughtime round
trips that place trusted time and, with them, `time.max_drift`. The enclave image launches with
Secure TSC. The guest's timestamp counter is the processor's, scaled to the frequency that the host
requests at launch by a factor that the SEV-SNP firmware sets and the host cannot change afterwards.
The guest kernel reads that frequency from the hardware.

- The trading server refuses to start when the hardware reports the guest counter's frequency as
  below 1000 MHz. Below that, the firmware has rounded the host's request below the unit the
  hardware counts in, and the kernel calibrates against clocks the host emulates. At 1000 MHz or
  more, the whole-megahertz reading differs from the true frequency by at most 0.1%.
- The launch measurement covers the Secure TSC setting. A verifier refuses a release whose stated
  `guest_features` lack it, and the launch digest, which anyone can reproduce by rebuilding
  `measurement.json`, ties that statement to the launch.
- The kernel keeps time on that counter, and kvm-clock, whose scale the host writes, is turned off.
  Nothing the host supplies after boot can mark the counter unstable, because the kernel has no
  frequency scaling and no ACPI processor driver.
- A trading server with real attestation refuses to start when the host has had the kernel mark the
  counter unstable at boot, which would move the kernel to a clock the host emulates.

[The enclave guest](enclave.md#the-guests-clock) describes the guest's clock in full.

Known limits:

- None of these steps has yet run on SEV-SNP hardware, which alone can confirm them.

## Public certificates

With `[api.tls.acme]`, a public certificate authority (CA) certifies the enclave's attested TLS key
for the API's domain, so a browser can trust the API without checking the attestation. The trading
server answers the CA's TLS-ALPN-01 validation itself. `engine ct-watch`, which runs outside the
enclave, raises the Critical alert `unattested_certificate` for a certificate for the domain whose
key no verified attestation names.

Known limits:

- A browser that does not check the attestation also trusts any other certificate a CA issues for
  the domain. The host can obtain one for a key outside the enclave by answering the CA's
  TLS-ALPN-01 validation on the port it forwards, and so can whoever controls the domain's DNS.
- The domain's CAA record narrows issuance to one CA and to TLS-ALPN-01. With sealed keys,
  `accounturi` records narrow it further to the ACME accounts the enclaves hold. These are the
  account sealed with the turn's keys and, with rotation on, the account sealed with each member's
  standby key. A host that deletes the sealed keys forces new ones, which the vault must register.
- `engine ct-watch` raises its alert only once a Certificate Transparency log that it follows shows
  the certificate, and only after a grace period, ten minutes by default, in which no verified
  attestation has named the key. It must be running before the first such certificate is issued,
  unless it is configured with a `start_index` that reads back far enough.
- It checks the signatures on tree heads, but not consistency or inclusion proofs, so a log that
  shows different views to different clients can hide a certificate from it. It sees only the logs
  it follows.

## Elections and daily turns

The operator registry elects the operator, and the operator in effect is the one that the registry
currently names. The admin key that signs the operator's instructions follows the registry's
elections, which the engine mirrors from finalised chain events. While rotation is on, the vault's
current release follows the operator in effect, and an elected release takes the enclave's keys and
the log from the incumbent's enclave from the election's effective time.

Once the registry names a schedule, the operator rotates daily among the members of a rotation set.
When an election closes, it adopts a rotation set, which takes effect seven days plus the finality
margin later. Each daily turn begins at a boundary at 22:30 UTC. From the turn's draw deadline, one
day before its boundary, until the boundary, anyone can draw the turn from the set in effect at the
draw deadline, with a seed relayed from Ethereum. A turn's admin key signs from its boundary only
when a set mirrored at least seven days earlier names it, counting from the first of the unbroken
run of mirrored sets that named its candidate with that key.

From the boundary, the vault names the turn's release, in the generation the schedule fixed, without
a transaction. Both enclaves of a handover accept the turn's member from the finalised schedule once
their trusted time reaches the boundary. From then on, the turn that the schedule names is the only
holding of the keys that either enclave accepts. The incumbent hands over only a turn its own log
accepted. Once it has stopped for a member whose transfer never confirmed, it hands over a turn that
the turns its log held at the stop show it will accept, up to seven turns ahead. A host that holds
back an enclave's Roughtime replies can therefore delay a handover, but cannot bring it forward.

A holder that the schedule names again in a new generation, after a turn that nobody took, seals its
live keys in that generation on its sequencing thread before it logs `TurnTaken`. No copy is
therefore sealed after it has stopped for a handover. A member earns the operator's share from its
enclave's `TurnTaken` to the end of its turn, and an incumbent past its turn earns nothing. A
promoted member holds the office until the next turn applies. The operator in office before the
first turn earns nothing after that turn applies, at its boundary or at its mirror if that is later.

Known limits:

- Elections are mirrored from finalised events, so the incumbent's host can delay an elected key's
  takeover. Only the custody watcher's stall alerts bound that delay.
- Whoever can withhold every seed relay until a turn's draw deadline chooses between that turn's
  relayed seed and its fallback, as [the seed relay](#the-seed-relay) describes.
- A member's listing and admin key are read when the draw lands. A deregistration, or a withdrawal
  of approval, that lands within the draw window therefore lets the drawer choose the outcome by
  ordering the draw around it.
- Nothing in the trading server draws turns. A turn that nobody draws leaves the member before it
  named.
- A turn drawn within a finality period of its boundary waits for finality, as does the handover
  after a promotion made within a finality period of the next boundary. The member keeps its copy of
  the log meanwhile.
- Turns are a day long. The `short-turns` feature, which makes a turn five minutes long and a set's
  notice one turn, is for development only. A `production` build does not compile with it, and a
  production trading server refuses to mirror a schedule whose `turnLength()` is not a day.
- A trading server activates its build's newest protocol version at boot, and an older build cannot
  read the log after that. Every member of the rotation sets must therefore run a release that
  implements a version before any member's trading server activates it.

## The seed relay

The seed relay carries the RANDAO mix that seeds each daily turn's draw from Ethereum to Arbitrum.
`SeedRelay` on Ethereum sends a seed only with a proof against a beacon block root that the EIP-4788
contract holds. The proof must show that:

- the block header hashes to that root;
- its state's `fork.current_version` is one that the deployment accepts;
- its slot lies after the epoch's last slot, and before `randao_mixes` reuses the epoch's index;
- the state holds the seed at that index.

`SeedReceiver` on Arbitrum accepts a seed only from the relay's aliased address, only for the epoch
that the schedule fixes for its turn, and only before that turn's draw deadline, from which its
answer for the turn is final. Relayers run in members' trading servers under `[seed_relay]`. A
relayer's beacon node, Ethereum providers and signing endpoint are reached through the host and are
untrusted. Each read that decides what it relays is a strict majority's or is checked again on
chain, so a wrong answer costs a relay, never a wrong seed.

Known limits:

- The relay trusts Ethereum's consensus for its block roots. The mix itself can be biased by the
  proposers of the epoch's last slots, each of whom can withhold a block and so choose between two
  mixes.
- The relay trusts Arbitrum's inbox for delivery. Arbitrum One's sequencer can hold a relay's
  message for 24 hours before anyone may force its inclusion, and a relay that misses the draw
  deadline is refused. The schedule therefore gives a relay two days.
- A turn with no relayed seed draws from a fallback, which is the last relayed seed hashed with the
  turn's number and with the hash of the Arbitrum block in which the fallback was first fixed after
  the deadline (`SeedReceiver.fixFallback`). No fallback is known while the set that draws from it
  can still be chosen. Whoever can withhold every relay until a deadline chooses between the mix and
  that fallback.
- Arbitrum's sequencer, which makes each block and orders every call, can steer a fallback turn's
  draw by grinding the recorded block's hash. It can also leave the turn undrawn, with the member
  before it named, by withholding the record for more than 256 blocks after that block. A relayed
  seed is unaffected by either.
- Once the block is made, its hash is public, and members' trading servers configured with
  `[liveness]` record it within seconds, whether they are following or holding a turn. Withholding
  the record otherwise takes a time when no such trading server is running.
- No turn draws before the first seed arrives. After the timelock replaces the receiver, as after an
  Ethereum fork, a turn with no seed in the new receiver draws from the last drawn turn's seed,
  hashed in the same way.
- Nothing relays by itself, and `[seed_relay]` is optional. A relayer raises the Critical alert
  `seed_missing` when a turn whose draw deadline has passed has no seed, once the receiver holds an
  earlier one.
- The relay's indices are those of the `BeaconState` from the Electra fork onwards, whose fields
  fill a 64-leaf tree, and it counts epochs from 12-second slots. It accepts only the fork versions
  it was deployed with, and the relayer hashes only Fulu states.
- A later fork stops relays until a release of the trading server reads its state and, unless its
  version was listed at deployment, a new relay and receiver are deployed. Turns draw from the
  fallback meanwhile.

## The log check before withdrawals

While rotation is on, and a turn is in effect whose set names two or more members, the enclave signs
a withdrawal only for an approval at or below the head of the log that the last boundary's check
verified. A check is another member's verification of the log through a checkpoint:

- At a handover, the check is the successor's own replay of the log through the checkpoint it was
  handed.
- At a promotion, the check is the promoted member's replay through the vault's checkpoint that the
  promotion fixed, unless that member sequenced part of the range, as after its own upgrade.
- At a boundary that its holder holds on through, the check is an attested verification bound to the
  boundary, the turn's assignment and the vault's signer epoch. The holder's enclave asks the
  members of the turn's order, first to last, in questions it signs, and a member answers only from
  the checkpoints it verified under that signer.

A member may answer only if it sequenced none of the instructions since the last verified head,
under any release, and runs no release that did. The log's `TurnTaken` instructions, the promotions
and skips that the schedule records, and the keys' own record of each release that handed them on
past that head decide this. A member promoted or skipped to after the assignment in office, a member
with no handover endpoint, and the holder's own candidate are not asked.

An approval made during a turn therefore waits for the next boundary, which takes at most a day plus
the time a handover takes. Payouts stay anchored to the roots that the vault accepted, and a member
of the set cannot be paid on a branch that no other member holds. The verified head is sealed with
the keys while the check applies, so a host that restores an older copy only delays payouts, and the
next check then covers the holding of the latest `TurnTaken`.

Known limits:

- A skip that a reorganisation removed is never mirrored. A promotion's standby keys, and an older
  copy of the keys that a host restores, record only the handovers that they themselves saw.
- A check binds a release, not a host. A host that also runs another member's release can feed that
  enclave the branch it withholds, and have it verified.
- The rule bounds neither delay nor censorship. A host that keeps its trading server from asking, or
  members that do not answer, hold payouts until one answers or until the next handover, and
  `log_check_pending` is raised after 30 minutes.
- A boundary whose turn nobody drew logs no `TurnTaken`, so approvals wait for the next drawn turn.
- A start from a snapshot past the verified head cannot say who sequenced the range. It asks no one,
  and raises `log_check_unattributable` until a full replay or a handover. A trading server set up
  to take no turns signs no withdrawal while the check applies, and raises `log_check_unavailable`.
- Instructions other than withdrawals stay exposed to a branch that the host withholds, for at most
  one heartbeat interval past the newest sequence number that another member confirmed.
- With a one-member set, with rotation off, or with no turn in effect, only the posted roots gate
  payouts.

## Liveness and promotion

The schedule records each turn's assignment, which names the member that holds the turn, its
position in the turn's order and the generation in which it holds it. Each member's enclave also
keeps a standby key, which another enclave vouches for and the member's candidate account registers,
so that the vault can make that key its signer in a promotion. Block time on Arbitrum is divided
into slots of one heartbeat interval, and anyone may mark a slot as witnessed while it runs.

**Heartbeats.** While a turn names the operator, the operator's enclave posts an
`engine.log-head.v2` head at least twice per heartbeat interval, as its heartbeat. The schedule's
deployment sets the heartbeat interval, the number of missed slots before a promotion
(`heartbeatMisses`) and the take window (`takeWindow`), within bounds that the contract enforces.
The proposed values are 60 seconds, five slots and two slots.

While the turn's set has another member, the enclave signs a head or a checkpoint only at a sequence
number that another member's enclave confirmed holding. The operator therefore runs, and posts
checkpoints, only while another member confirms its log. It stops accepting instructions once its
log runs more than one interval past the newest confirmation. A checkpoint is the log entry after
its sequence number, so the operator stamps nothing past a checkpoint until another member confirms
that sequence number. The pause lasts about one stream delivery, one sync and one round trip.

**Promotion.** Once `heartbeatMisses` witnessed slots pass with no head of the signer epoch, anyone
may promote the next qualified member in the turn's order. The vault then moves its signer to that
member's vouched standby key, in a new signer epoch, with no timelock. Detection takes
`heartbeatMisses` to `heartbeatMisses + 1` intervals after the last head that landed, about six
minutes at the proposed values, and at most 10 minutes under the deployment bounds. Each promotion
adds the promoted member's start, and each member in the order that fails to start adds one more
detection window.

A promotion names the first member in the order that has a promotable standby key and did not hold
the keys under an assignment of the turn. It prefers a member that held nothing of the turn to one
that was drawn or skipped to but never received the keys, and it names the key holder last. The key
holder is the candidate whose assignment recorded the release that signed the signer epoch's newest
head, which no release move of its own changes. A holder that withholds the keys therefore cannot
use up the order and take the turn itself.

**Skips.** A turn's member takes the turn with its first head under its assignment, which `LogHeads`
latches. Once `takeWindow` witnessed slots pass with no latch, counted from the latest of the
boundary's slot, the assignment's and the last signer change's, anyone may skip the turn. The skip
passes the turn to the next listed member in its order that has held no assignment of the turn,
other than the key holder, in a new generation with no signer change. A turn whose current
assignment is a promotion is never skipped, since only missed heads move the turn on from a promoted
member. No skip happens while the guardian has retired the signer either, because no head can latch
then.

The deployment's minimum take window exceeds the handover's switch budget plus one interval plus a
head's inclusion, which the head tolerance bounds, at every interval the bounds allow. The
deployment sets the switch budget to cover the measured switch, plus the holder's and the
successor's reads of the turn's latest assignment after the stop. Each read takes thirteen round
trips to the custody providers in turn, or fourteen while a provider's tip is past the agreed block.
The take window and the heartbeat rule in force always leave the first skip's member time to take
the turn before the promotion for an untaken turn could pass. The member can sign its first head
before its trading server stops, which happens 15 seconds ahead of any promotion.

**An untaken turn.** Once `heartbeatMisses + 1` witnessed slots pass after the later of the
boundary's slot and the last signer change, with the turn's assignment unlatched in a set of two or
more members, anyone may promote the next qualified member. This holds even while the incumbent
posts heads, which bind the incumbent's own earlier turn and latch nothing. The incumbent, which
holds the keys, comes last. Its own standby key, which fences its old key out, is promoted only when
no other member qualifies.

Who qualifies does not move either count, so no listing, release or standby change brings a
promotion earlier. With no member qualifying, the promotion is refused after the incumbent has
stopped, and the governance fallback below applies. An unwitnessed slot restarts both counts, so a
chain outage across a boundary gives the member a fresh window.

**Handing over after a skip or promotion.** Skips and promotions act once Arbitrum's sequencer
confirms them. The key holder admits, and the member named takes, the assignment that the custody
quorum's latest agreed block shows, bound to the signer epoch, the turn, the assignment's position
and its generation. The holder reads it again before it stops and before the keys go, and the member
reads it again before it seals them. A reassignment in between therefore leaves the keys with the
holder, which then serves the member named.

A skip's member is admitted once the holder's log shows the skip's set naming it with notice, before
the log mirrors the skip at finality. The member's own upgrade to another approved release waits for
that mirror, so the boundary's log check never counts the member's instructions as another member's
to check. The member logs `TurnTaken` only once the mirror applies the skip, and the operator's
share for the time before that goes to stakers.

A member that took the keys and was then skipped keeps sequencing until it hands them to the member
now named, and earns the operator's share only until the log mirrors the skip. A holder whose skip a
reorganisation removed hands the keys to the member the schedule names again. A skip's member with
no copy of the log, or one restarting, reads the skip at its start in the same way, before the skip
is final.

A holder that the registry no longer names, such as one that a later skip or a reorganisation moved
past, does not restart as a holder, just as an incumbent past its boundary does not. It follows, its
keys stay sealed, and the turn moves on by a later skip or by the missed-head promotion. A running
member that handed its drawn turn to a skip's member takes the turn back once a reorganisation
removes the skip.

**The governance fallback.** Once no position in the order qualifies, the order is exhausted, and
the governance fallback applies. The timelock turns rotation off, and then installs an operator with
`installOperator` once the drawn turns have ended. The fallback takes at most the 10-minute
detection bound, plus the timelock's delay, plus up to two turns for the drawn turns to end. At
launch values that is below the 14-day exit period.

**The promoted member.** A promoted member starts at a block that the custody quorum agrees on,
before finality. It serves, from its first answer, the public certificate that its standby TLS key
obtained while it followed, so clients and following members verify it against public roots. A
member whose standby key obtained no certificate serves a self-signed one, and orders a public
certificate (`certificate_pending`).

The outgoing operator, once it returns as a member, sets aside the instructions its key sealed past
the last head its signer epoch posted, or all of them if it posted none, once a final block shows
that epoch ended. It then rebuilds its copy from the operator in effect, and raises
`superseded_tail_set_aside`.

**The old operator's stop.** The old operator stops by its own clock before the earliest promotion
on either path could pass. This assumes that Arbitrum's block timestamps lead true time by no more
than 10 seconds, and that its monotonic clock runs no more than 500 ppm slow. It also stops whenever
its reads cannot rule out a promotion within the 15-second margin, as through an Arbitrum sequencer
outage, or a lost custody quorum lasting about `heartbeatMisses` intervals. It resumes on its first
read after one.

For a set with no other member, a read showing that no promotion can pass keeps the operator
running, without a newer read, until 15 seconds before the later of the next turn boundary and
`promotionDueAt`. With no turn naming it, or no schedule, such a read keeps it running until 15
seconds after the first boundary at which a drawn turn could start. That boundary is a day after the
earliest rotation set takes effect, or at least eight days ahead if no set is in effect or waiting.
Until that boundary, from which another set may apply, such a read lets checkpoints pass
unconfirmed, and heads go out at the tip of its own set's turn only.

A signer that the guardian retired ends no run, but its heads no longer land, and `promotionDueAt`
goes on counting the retired epoch's deadline. The operator therefore fences itself once that
deadline nears. A signer that the timelock then installs starts a new epoch, with a fresh count.

Known limits:

- An Arbitrum reorganisation of the block at which a promoted member started can remove its
  promotion after the member has traded. The member then stops and raises `promotion_reorged`. It
  resumes only if a final block records the same promotion with the same commitments, and otherwise
  sets aside what it sequenced, and the key.
- Two copies of one key, which only a host keeping or restoring a sealed copy can run, can serve
  together until the losing copy reads the conflicting posted head. If the host blocks the losing
  copy's reads, they can serve together until the stop time that copy last computed, so one interval
  does not bound the overlap.
- A host that rolls its member back within the unposted window, keeping the answering members from
  seeing what the member signed, can make the member sign a second history over at most that window.
  That is the member's fault, and [fault proofs](#fault-proofs) make it provable.
- A heartbeat proves that the enclave sequences, not that clients are served. A host can keep its
  own turn halted with held heads, through a benchmark-sync hold or a trusted-time halt, until the
  turn ends.
- A held head names the newest sequence number that another member confirmed, not the log's tip. A
  trusted-time halt stops the signed stream, so a tip that no member had received by then could
  never be confirmed, and requiring it would promote a member through every such halt.
- A held head can therefore trail the log's tip by the instructions that no member received before
  the hold, at most one interval's worth. Those instructions stay exposed as any unconfirmed
  instruction is, and a hold stamps nothing past them.
- A standby key's registration is sent by the candidate's own account, so a candidate that does not
  send it is not promotable. The operator nonetheless counts that member's confirmations on the key
  once its enclave has verified the key's attestation.

## Fault proofs

Anyone can submit to `FaultProof` two `engine.log-head.v2` heads, signed under one signer epoch and
binding the same turn, position and release, that name different digests at one sequence number.
`FaultProof` checks both against that epoch's signer and the schedule's assignment, once per
assignment. The engine then cuts the assigned member's operator share by a haircut that starts at
the recorded penalty and falls linearly to nothing over its decay period. The withheld part goes to
stakers.

Known limits:

- Heads under different signer epochs, such as the history a promotion discards, prove nothing.
  Neither do heads of different assignments, or a second history whose heads never share a sequence
  number.
- An answered instruction left out of the published log proves nothing, and neither do censorship
  and delay.
- No self-bond is slashed.
- A fault proven before the log recorded the fault proof contract is credited and cuts nothing.

## Parameter votes

While votes govern, the governed parameters, listings, the toxicity score's weighting, closing
spread cap adoptions, corporate actions and protocol activation change only by a proposal that the
rotation set passes on the `ParameterVote` contract, with more than two thirds of its snapshotted
weight. So does the access policy, which a vote can record only in a log that holds none. The engine
mirrors passed proposals from finalised events, and refuses the operator's own instructions for
them, including its notices.

Votes govern only while a rotation set with members is in effect, since only its members can
propose. From rotation's start until the first set takes effect, and while an empty set is in
effect, the operator keeps its bounded instructions. A drawn turn keeps votes governing from its
boundary until an installation. Each mirrored staking frontier carries the registry's turn state at
that finalised block, read through the custody quorum, including a drawn turn that has yet to start.

The vote contract carries no limits. The engine holds each voted change to each field's validation
and to these bounds:

- the close-out level within 50% either side of its launch value;
- closing spread caps lowered only, and kept at or above the market's launch closing charge;
- each spread band between its launch rate and twice that rate, each margin rate at or above its
  launch rate, and no stress level's size multiplier below 1x;
- each toxicity score tier within 50% either side of its launch value, with the markout floors
  fixed;
- the base profit fee between its launch value and 50% above it, and the fee curve at or above its
  launch curve at every holding time, reaching its base rate within 30 minutes at most;
- the launch volume tiers, with no threshold or day divisor below its launch value, no best-price
  share above it, and each tier's spread between its launch spread and twice it;
- each launch house loss threshold kept at its level or above, and at most its launch value;
- each tier's margin floor at or above its launch value;
- each tier's order limit and request rate named, and at or below its launch value where the launch
  values set one;
- each tier's fee discount, financing discount and emissions boost at or below its launch value;
- loosened price rules within the launch rules, and corporate bounds no looser than their launch
  values;
- a corporate action within those bounds, except for the limit on a dividend's share of the
  reference price and the notice for a split;
- each market's minimum take profit and stop loss distance between its launch value and twice it.

Known limits:

- No other range applies. Members holding more than two thirds of the weight can therefore set any
  other parameter to any valid value after seven days' notice. A normal change applies only to the
  parameters or market it passed on.
- A fast-path proposal applies at once, but only as a restriction that moves fields towards lower
  risk. It lapses at the recorded lifetime, at most seven days, although another fast-path vote for
  the same instruction renews it.
- A fast-path margin raise is bounded only by each band's validation. It can therefore close out
  leveraged positions at once, with no notice.
- The trading server sequences a normal change once its notice has run, so a host that withholds
  instructions can delay the change as it delays any instruction.
- A voted access policy that a member's configuration does not name stops that member's trading
  server from starting on the log. Every member therefore moves to a release whose configuration
  names the policy before it is proposed, as for an instruction that a release added.
- In a log that holds rotation sets when votes first govern by set, rotation counts as on until the
  first mirrored staking frontier.

## Development settings

The development settings exist for local testing, and make no security claims of their own. They are
the mock enclave, mock custody and mock prices; `POST /v1/dev/deposit`, which the trading server
serves only with mock custody; and the browser verifier's `verifyMockAttestation`, which reads a
mock attestation and reports it as one. A weakness in them is in scope when a production
configuration can reach it, or when whoever runs an attested enclave can switch them on.

A production configuration uses hardware attestation, on-chain custody, live prices and signed
requests. A `production` build refuses every mock provider. Every request that acts for an account
requires its signature, whatever the providers. The development configurations name a published
signer for the mock enclave, which real attestation refuses, and
[`config/test-keys.toml`](../config/test-keys.toml) lists every key whose private key the repository
publishes. A trading server whose enclave is not the mock refuses each of them as its operator key
or screening source.

