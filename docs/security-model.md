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
  [MONKEY staking and distributions](#monkey-staking-and-distributions),
  [the move to a new vault](#the-move-to-a-new-vault)
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
an eligibility declaration. Declarations, orders, closes, withdrawals and payout reissues each need a
fresh decision of the screening source that the log records, about the account and the client's
network address. The decision must be signed by the authority the log records, and it must name the
account. The engine accepts it only while its observation time is no older than the recorded
freshness bound, and no more than a second later, than the time the trading server stamps the
request. The trading server keeps one decision for each account and client, and attaches it to each
of that pair's requests while it is fresh. A client is its address, or for IPv6 its /64, as the API's
limits count it. It asks the source again before the decision runs
out for a pair that sent a request within the bound. A deposit's decision instead names the deposit
that it screens, and backs only that deposit. A deposit from a payer confirmed as sanctioned is held
uncredited, and the trading server publishes the evidence for the hold.

A request's decision proves that the recorded source classified the account, and a client address,
as the decision says, at an observation time within the bound before the request's stamp. Unless it
names the request, it does not prove that the source was asked about that request, so:

- A change in the source's classification, such as a new sanction or a client in a restricted
  region, reaches an account's requests only when the trading server next receives a decision for
  it. Until then, the earlier decision keeps backing the account's requests, for at most the bound
  after its observation time.
- The host, which can delay or drop the trading server's calls to the source, can hold back a newer
  decision for as long.
- A refused decision is reused in the same way, so an account stays refused until the trading
  server receives a newer decision, and the source is not asked again on each of its requests.

A log written before protocol `V85` binds each request's decision to that request, as the
[version list](../crates/core/src/protocol/versions.rs) describes.

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
- The API's screening slots are shared in the same way. They bound the calls to the source that
  the trading server makes, when a request finds no fresh decision and when it refreshes one. Each
  account and each client address holds at most its share, a production trading server has at least
  68 slots for each second of the source's timeout, and a reserve of them goes only to requests that
  the account's current state shows can reduce risk, such as closing a position it holds or
  withdrawing cash it has, one at a time for an account. Refreshes never take the reserve, nor the
  last slot outside it. A client can still hold every slot while the source is answering if it has
  enough addresses and credited accounts: at the production values and a source that takes its full
  2 seconds, 68 accounts behind 34 addresses, two of the accounts holding a position or withdrawable
  cash.
- The trading server keeps decisions for at most 50,000 pairs of an account and a client at once. Past that, a request from a new pair waits for the source each time, as no decision is kept
  for it.
- A hold for a confirmed sanction has no automatic release or refund.
- Jurisdiction screening uses the address of the connection's peer or, behind a configured
  pass-through proxy, the client address in the proxy's PROXY protocol header. The host's network
  supplies both addresses, so neither is bounded against a host that relays or translates client
  connections.
- The log carries no client address, so a replay cannot check which address a decision was made
  for. The trading server attaches a decision only to requests from the client that it asked the
  source about: the same address, or for IPv6 an address in the same /64. No check in the engine
  establishes that.
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
processes. A notice can be used from its time until seven days after it, in sequenced time; a change
it names needs a new notice after that, so no notice stays armed for the operator to use at a moment
of its choosing. The bounds are:

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
- The stress parameters and a listed market's financing markup change only after seven days' notice.
  The operator can hold a stress floor of at most level 1, which lapses after 72 hours. The
  toxicity score's tiers change only after seven days' notice, and stay within 50% either side of
  their launch values.
- A signed benchmark rate moves from the rate already applied by at most 300 basis points for each
  calendar day between their dates, and a market that holds positions stays on its benchmark.
- A new operator key takes effect seven days after its rotation is logged, and the old key then
  signs nothing.
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
- A listed market's price rules change only after seven days' notice, tighter as well as looser and
  whether or not the market is reduce-only, and are never loosened beyond the market's launch rules.
  Rules under which a market that holds positions would have no price are refused, reduce-only or
  not, noticed or voted. A change outside a vote with no notice, or price rules loosened beyond these
  bounds, is in scope.
- A futures market's contracts, from the one it prices from on, change only after seven days' notice,
  though a contract can be added after the last at once to a market that holds no positions. A
  contract renamed, dropped or moved, or added to a market that holds positions, outside a vote with
  no notice is in scope. A market with no gap recorded to its next contract rolls onto it
  once its front contract has had no price for seven days, at the first command past the next
  contract's roll time at which the next contract has a price, from the market's last mark, so the move
  between the last mark and that price is never booked. The seven days count sequenced time, the
  market's closed hours included, only while a next contract is listed, and start again whenever the
  next contract changes. A front that stops being quoted before a gap is measured therefore leaves the
  market without a price for at least seven days, and longer when the next contract is listed after
  the front stops. While the front's venues quote it, a roll from the mark takes seven days in which
  the front has had no price, closed hours included: an outage of those venues together with the
  closed hours before, within and after it. A market rolled from its mark sooner is in scope.
- A calendar change by the operator cannot close a listed market within seven days at any time its
  calendar in force has it open, whatever the market's weekend margin and whether or not it holds
  positions. The bound does not hold back a change that only adds trading hours, or one that closes
  the market only from seven days after it is logged, though the weekend closure bound below and a
  pending dividend or split still apply. A market's first listing may name any calendar. A vote's
  calendar change waits the vote's seven days, and then may close the market at once. A calendar
  change outside a vote that closes a listed market within seven days is in scope.
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
- For a market with a weekend margin, a calendar change by the operator also cannot narrow or remove
  a weekend closure that starts within seven days.
- A weekend margin applies to new positions and withdrawals from two hours before a weekend closure
  until the market reopens. An account below it when the closure starts is closed out in that market
  at the market's last price. A weekend margin change beyond these bounds, or a position held across
  a closure below its weekend margin, is in scope.
- The house loss is the house's net asset value less the net capital moved into it. Once the house
  has lost money to its clients over the trailing day, every market rises to the stress level that
  the house loss trigger sets. The stress levels keep that trigger at its launch level or above,
  with a threshold no higher than its launch value. A trade or close-out executed below that level
  while the house is behind on the day is in scope, and so is a change beyond this bound.
- While the parameters name offset groups, close-outs read each account's portfolio requirement: its
  per-position requirement at the market's bands, less each group's credit on the exposure its
  positions on one side match on the other. That requirement is never above the per-position one,
  and never below each group's floor share of its matched part's. A close-out closes, one step at a
  time, the position or group whose closing restores the account with the least value closed, a
  group's positions together while all of them can trade. Each step is judged at the stress level and
  volume tier its own trades pay: a group's later positions at the house's net asset value, valued at
  the marks, and at the volume tier, each as the group's earlier trades leave it. Every account that a
  trade, a triggered order or a level fills is checked for close-out once the instruction's trades are
  done. The groups, their members, loadings and bounds are fixed at launch. Each residual shock and
  floor share moves only up, within its bound, after seven days' notice or by a vote, and a fast-path
  vote may raise a floor share. The grid's shocks are the same at every time. A market given a weekend margin raises
  only its own bands over its weekend closures, and each group keeps its credit: an account below that
  whole requirement when such a closure starts is closed out in the closing markets at their last
  prices. A market without a weekend margin has no weekend close-out. New risk and withdrawals read
  the same requirement at initial bands, each band at least the account's tier floor and a group
  credited nothing while a member the account holds cannot trade; an order that only reduces is held
  to it when it would raise the requirement judged with every group's credit kept, a shut member's
  included, while a close-all, a triggered order, a level and a close-out never are. An account left open below its close-out level after an instruction, a close-out of an account
  above it, a fill or withdrawal that leaves equity below the initial requirement it is held to, or a
  requirement or grid beyond these bounds, is in scope.

Known limits:

- The house loss trigger caps how fast the house loses, not what a fast client takes from the
  house's income. [The loss-rate trigger study](../research/loss-rate-trigger/README.md) gives the
  bound and the kinds of trading it leaves open.
- The profit fee follows each position's own holding time, and nothing links one account to another.
  Opposite positions held in two accounts can each age past the fee curve's last holding time. A
  client who then closes one leg as the price moves, and the other moments later, pays the base fee
  on a gain the curve would otherwise have taken. Offsetting positions in correlated markets within
  one account do the same, at some basis risk. The toxicity score's spreads and the house loss
  trigger bound what such trading takes.
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
but the seven CME-traded futures markets needs fresh quotes from two venues, so a delay beyond the
bounds below on either venue leaves the market without a price. Those seven (US500, US100, US30,
JP225, XAU-USD, XAG-USD and WTI) take `min_sources = 1` and price from Massive's futures feed alone,
because Massive's futures data derives from CME's. They have no
price while Massive is unavailable, and no second source checks a Massive print, so a bad print
within `max_age_ms` sets their price. Two connections to one venue, such as Coinbase's Advanced Trade and Exchange
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
- A rotation-set member opens its keyless venue connections a minute before its own turn, and holds
  their quotes back until its sequencer runs. A quote kept that way is held at its stamp to the same
  `max_delay` and `max_age_ms` as any other, so it gains no time. A connection that carries a key
  opens when the turn starts, since a vendor allows one live connection per account and the members
  share it, so those markets have no price for a few seconds at each handover.

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
  the ping, as they were measured to do. The Massive, TraderMade and Alpaca connections
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
and posts it to the vault. A root signed before its log activates protocol `V91` leaves out part
of each receipt-backed pending or held deposit's record: the reference the record names inside it,
and a held one's screened payer and the reason it is held. The signature over a daily snapshot's
whole encoding binds them (below). Anyone can replay the log with this repository's code, and check
the signed root at each checkpoint. Replies, stream entries and signed withdrawals leave the trading
server only once the batch that holds them is durable on disk.

With `[checkpoints]` configured, the trading server verifies its replayed log at startup against the
latest commitment posted to the vault, at custody finality. It refuses a shorter or conflicting log
before it serves anything or opens its history for writing. Checkpoint signatures are verified
against their chain, their vault and their named signer. Each new checkpoint also signs the exact
sequence number and root of its predecessor, which the vault compares atomically, so two publishers
with one key cannot both extend the vault's chain.

The enclave seals each instruction: it signs a chained digest of the instruction, its stamp and its
events, chained to the digest before it back to the last checkpoint, whose sequence number and root
start the chain. `engine replay` computes every instruction's chained digest, and checks that every
seal is well formed: a 65-byte signature with an Ethereum recovery id, both scalars in range and a
low `s`, a signer that is not zero, the log's deployment once the replay knows it, and logged
events. It checks a seal's signature in full only at the first and last instruction of each run. A
run is the longest stretch of consecutive instructions in one checkpoint interval whose seals name
the same signer and deployment. A run ends at a checkpoint, at a seal that names another signer or
deployment, at an instruction with no seal, and at the end of the log, so a run never spans a change
of the signer its seals name. The run's last seal signs a digest that chains every instruction,
stamp and event set since the checkpoint, so when it verifies, its signer chained exactly those
instructions in that order, barring a keccak-256 collision. An instruction altered, inserted,
dropped or reordered after it was sealed therefore fails the replay at a seal of its run or of the
next run in the interval, and the error names that seal and its run. A run whose seals were
relabelled to name the signer of the run after it, so the two merge, fails at its first seal, and
one relabelled to name the signer of the run before it fails at its last. The replay checks the open
run before it reports any later fault, so the run's failure comes in log order. It applies a run's
instructions before the seal that authenticates them is checked, which changes only where a fault is
reported, since an audit acts on nothing. `engine replay --every-seal` checks every seal in full, as
below. It cannot be combined with `--snapshots`, whose ranges each check seals only at the ends of
their runs.

`engine replay --snapshots <dir>` replays a log in ranges, several at once, from the daily snapshots
in `<dir>`. A trading server with `[daily_snapshots]` keeps the engine's state at the first
checkpoint of each UTC day and serves it beside the archive. Beside the state, each snapshot carries
what a replay of the log holds after that checkpoint and folds from every entry before it: the
instruction count, the running digest of every instruction and the latest instructions' digests,
the checkpoint accounting, the genesis progress, the protocol activations, the sequence number of the
latest parameters, every checkpoint's signer and root, the turn takings, the withdrawal approvals in
payout order with their latest authorizations, and the approvals a checkpoint deferred. It also
carries the dashboard record at the checkpoint and the chain anchors of the trading server's chart
history there. The trading server's enclave key signs each snapshot over the deployment, the offset
of the snapshot's checkpoint in the log, that checkpoint's root, and the SHA-256 of the state's
encoding, of the replay state and of the anchors, each as written; a start requires the signature
(below), and a replay in ranges does not read it. The replay splits the log at each snapshot's
checkpoint, and starts a range from a snapshot only where the log holds a checkpoint at the
snapshot's offset equal to the snapshot's, which it checks as it checks every checkpoint, and only
once the root it computes over the snapshot's state, each market's index of resting orders rebuilt
from the accounts' orders, is that checkpoint's; a snapshot of a state before `V27`, or before
custody finality, is refused. A snapshot of a checkpoint the log does not hold, as one a trading
server wrote in a tail it later set aside, starts no range, and the replay names it. When the
directory holds snapshots that carry the replay state, only those split the log, and each range from
one goes on from its state and its replay state as a whole replay would hold them there, through the
checkpoint where the next range starts. The replay accepts the ranges only where each starts at the
checkpoint the range before it ended at, from the state that range ended in, compared by the SHA-256
of the whole state's encoding, which binds the parts of the state the root leaves out too, and from
the replay state that range folded, compared by the SHA-256 of its encoding. So a snapshot whose
replay state is not the log's fails the replay, naming the snapshot, and a replay in ranges applies
the log as one history from genesis, as a whole replay does, barring a SHA-256 collision, and reports
the fault of the earliest range that holds one. A directory of older snapshots, which carry no replay
state, still splits the log: there, what a whole replay carries across a checkpoint besides the state
is reset at every checkpoint, such as the seal chain, or recomputed from the ranges' own results in
log order, such as the checkpoints that typed withdrawal authorizations name and the approvals
deferred before a range, and the replay does not compute the running digest of every instruction or
the checkpoint accounting. A snapshot crafted so that building its indexes or its root overflows the
engine's arithmetic stops the replay with a panic rather than an error; it never passes.

With `[archive]`, a trading server uploads each daily snapshot to the bucket as soon as it publishes it,
and removes its host's copy of one only once a read of the object from the bucket's public URL, over
TLS that ends in the enclave, gives back the length and SHA-256 its manifest line names, and the
snapshot is stamped more than `daily_snapshots.local_window` before the newest, keeping the newest of
those. Its manifest keeps every line, and it answers a request for a snapshot its host no longer holds
with a 308 to the object. `engine replay --snapshots <URL>`, a trading server's public origin, downloads
every snapshot the manifest lists at or before the log's last checkpoint, from the server or from the
object a 308 names, each only as the bytes its line names, and replays from them as from snapshots on
disk; a rotation member's start follows a 308 only to the bucket its own `[archive]` names, where it
names one, and otherwise to any object of the snapshot's file, which it checks as any other. The snapshot
files and the manifest are the host's. A host can remove or alter a snapshot before its upload, as it
can remove one at any time, and the upload sends only the bytes the file's line names, so an altered
file creates no object. A line whose file name is not the one its offset gives names no key: however
many lines share that name, none is uploaded, read or removed, and the name pages
`daily_snapshot_unreadable`. Wherever a reader takes a snapshot from, it checks it as above, and it
fetches, writes or redirects to a snapshot only under the name its offset gives.

A trading server starts from its newest usable daily snapshot, no later than the newest one whose
frozen copy of the chart history it holds, and the log's segments on its host after the snapshot's
checkpoint, and reads none of the log before it; nor does a rotation member's
replica, a standby's copy, a handover successor's copy, or the re-derivation of the financing rows of a
rollover after the checkpoint. Two reads still need older segments: a posted log head older than the last 65,536 instructions
before the snapshot's checkpoint, whose links the snapshot carries, is checked by hashing the log's
instructions from the head onto the checkpoint, and a rebuild or repair of the account history index
reads the log from its start. So the host keeps the segments from the earlier of the posted head and
the snapshot's checkpoint, and every segment while the history index may need rebuilding. A usable
snapshot is one
that carries the replay state, whose checkpoint the log holds signed, its signature verified, by the
key the vault names at its sequence number (the signer of the checkpoint the vault accepted there, or
else of the checkpoint the vault finalized, which the snapshot rests on; under mock custody, the
trading server's own key), whose own signature is
by that key, so a key the vault has since retired vouches for none, whose checkpoint is at or before
the checkpoint the vault finalized and, where the vault accepted one at that sequence number, has the
root the vault accepted, and whose checkpoint the log on the host holds at the snapshot's offset. The
signature is checked before the state or the replay state is decoded. The start then computes the
root over the snapshot's state, which must be its checkpoint's, checks that the replay state stands
for that state and checkpoint, and replays the log after the checkpoint as a whole replay does: each
instruction's seal from the checkpoint's link on, each later checkpoint's signature and root, and
each later withdrawal authorization, whose checkpoint may be one the snapshot carries; for a recovery
pinned to the vault's signer history, a checkpoint before the snapshot that such an authorization
names is taken from the vault. What the start takes on trust is the state and the replay state at
the snapshot's checkpoint, on the word of the key that signed that checkpoint; the log after it is
checked as before, and a posted log head at or below the checkpoint is bound to the running digest
the snapshot carries. A snapshot whose signature, root or checkpoint does not pass is passed over for
the next newest, three at most; one whose state's root or replay state then does not check gives way
to a whole replay, as does the start when none passes; each is paged with Warning
`snapshot_start_refused`. Its dashboard record continues from the one the snapshot carries, and its chart
history from a frozen copy of the cache the marks task wrote at the snapshot's checkpoint, which the
start takes only when every chain matches the anchors the snapshot carries and the copy's rows are the
ones whose root the snapshot carries. That root is a Merkle-Patricia trie root over every row, keyed by
its market, resolution and bucket, which the marks task computes from the copy after checking each
row's tag under the key that wrote it. The start computes the root again from the rows as it imports
them, in one transaction that commits only when the root matches, so a row that the host changed,
added, removed or moved, before the start or while SQLite reads the file, leaves nothing imported. The
check needs no key, so a start under new keys on the same host keeps the chart history. A snapshot
written before it carried the root is taken only when every row authenticates under the cache's key. A
copy that does not pass is set aside with Warning `marks_cache_rebuilt`, and the chart history before
the snapshot is then lost until a provider backfill restores what it can.
The marks task writes the copy after the marks of every instruction through the checkpoint and before
any later one. With `[snapshots]`, a start with no recovery cache that verifies replays the whole log,
as it rebuilds the account history index from the whole log, which must index only what the start
verified; with `[daily_snapshots]` too, a start takes only a recovery cache that carries the replay
state, so the daily snapshots go on after it.

The segments before the snapshot's checkpoint are what the host says they are, but for those the
trading server checks: a replay from the snapshot reads none of them, and without `[snapshots]` no
signed catalog vouches for them. Once it has started, the trading server checks, on a thread of its
own, the instructions before the checkpoint whose links the snapshot carries, the last 65,536, that
the log on the host holds, from the first after the activation of the protocol version that keeps
every withdrawal approval in the engine: each must extend the link before it as the snapshot says,
which binds its bytes, events and seal; each checkpoint among them must follow its own instruction,
with the root the snapshot carries for it, and none the snapshot carries may be missing; each
withdrawal entry must follow its approval and the checkpoint its authorization names, with that
checkpoint's root, where both are among the instructions checked; and each checkpoint and withdrawal
entry must be signed by a recorded signer. A whole replay checks no more of where a withdrawal
authorization sits, and neither checks that every one is present. Nothing the trading server vouches
for is read from before the first instruction it checked, which is the one after the snapshot's
checkpoint until the check ends: a stream's backfill from earlier is refused with 400, naming the
public archive, as one from before the log's first segment on the host is, and a financing charge's
rows are checked against their file only from there on. A rotation member or a standby whose copy
trails that instruction is refused too: it then fetches the completed segments past its copy's head
from the public archive and its mirrors, takes each entry after the head only once its seal and link
continue the copy, as a streamed entry's must, a member's replica applying it first, and follows the
stream again from the new head, which a segment of 10,000 instructions brings within the checked
range. The archive, whose answers are not signed, serves the segments as the host holds them, which
anyone checks by a replay. A rotation member's or a standby's copy fetched from an archive mirror may
hold altered segments before its snapshot. It never serves them, and a whole replay of the copy, as
when no snapshot passes, refuses them.

What each replay trusts: a whole replay trusts, for checkpoints and withdrawal authorizations, the
signer it pins, the attested signers, or the vault's signer history, and computes everything else
from the log. A replay in ranges trusts the same, and nothing a snapshot says: a snapshot changes
only where a range starts, and its replay state must be the one the range before it folded. With
`--custody-config`, no range starts after the vault's finalized checkpoint.

A replay in ranges is projected, not yet measured, to take about 25 minutes for 30 days of launch
flow, the expected mix of prices and orders, at 100,000 accounts, about 246 million instructions, on
a machine with 20 physical cores and 64 GB of memory, with `--signer` alone or `--attestation`. The
projection takes about 79 CPU-microseconds an instruction with run-end seal checks and about 40
CPU-seconds to load and root each day's snapshot, about 20,800 CPU-seconds in all, and assumes each
range keeps about 1.6 cores busy as one replaying alone does. The default runs one range for every
two physical cores, ten here, each taking the next day as it finishes. A range replays no faster
than one core applies its instructions, so more cores shorten the replay only while there are days
to share among them, and ranges that slow each other lengthen it: at 10,000 accounts, an hour of
flow replayed whole in 78 seconds and in eight ranges on seven jobs in 32, on a shared machine. Each
range holds an engine of its own: seven at 10,000 accounts peaked at 0.9 GB together, so a range at
100,000 accounts holds about 1.3 GB, and memory bounds `--jobs` as cores do. With `--custody-config`
the replay first scans the log for the checkpoints to ask the vault about, reading its segments one
per core at a time and parsing only what are not instructions: at 10,000 accounts it took 0.2
seconds where a whole read took 18, and at 100,000 accounts it reads about 390 GB from disk, a few
minutes at a fast disk's speed.

A trading server's recovery, the log-head prefix pass, the startup history rebuild, a member's
replica build and a taken copy's replay check every seal in full, since each goes on to sign over,
or hold as its own, the running digest of every instruction that log heads carry (below), which
reads each seal's bytes. A replay that checks every seal checks them on a pool of threads, one per
core unless `node.replay_workers` or `engine replay --replay-workers` sets another number. A second
reader of the log's files finds each sealed instruction ahead of the replay, and the threads check
its seal while the replay applies the instructions before it. The replay applies an instruction only
once its own seal has been checked, and in log order. It uses a thread's verdict only for the seal
and the chained digest that it computed itself from the instruction it applies. Any other seal it
checks itself, and once the second reader passes an instruction the replay never reaches, it checks
every later seal itself. A host that serves different bytes to the two readers therefore changes
only which thread checks a seal, and a seal that fails stops the replay at its instruction with the
same error. A log that cannot be read a second time, such as a pipe, is checked entirely by the
replay's own thread.

A replay of a whole log, as `engine replay`, a recovery and a taken copy's replay read one, also
makes its own read, which checks and parses the log, on a thread of its own, and applies each
instruction on another in log order, so a fault in the log stops the replay at the same entry with
the same error as a read on one thread would. It computes its state roots on a third thread once the
engine holds its state tree. At each checkpoint it reads that root's leaf writes from its own state
and hands them to that thread, then applies the next instructions while the thread hashes them. It
compares each root with its checkpoint's before the next checkpoint, before it reports an error of
its own or of its read, and before it finishes, so a root that differs fails the replay at its
checkpoint with the same error, ahead of any later fault. By then the replay may have applied up to
one checkpoint's instructions past it, and a recovery may have seeded its marks cache and dashboard
record from them; it serves nothing, and its next start seeds both afresh. A member's replica, which
answers for each checkpoint it verified as it passes it, compares each root at its checkpoint.

With `[log_heads]` configured, the enclave also signs log heads. A log head states the sequence
number of the newest instruction the trading server has released, and a running digest of every
instruction up to it. Checkpoints do not reset the digest, so a posted head commits to the whole
history. A dedicated relayer posts each head to the `LogHeads` contract. The contract checks the
head's signer against the vault's `checkpointSigner` and `signerEpoch`, and keeps the highest
sequence number for each signer epoch, which is the period during which one signer is installed in
the vault. Each post names the previously accepted head it extends, so a longer divergent copy
cannot replace the anchor without containing it.

The digest reads each instruction's seal as the 65 bytes that its signature holds, however a copy
writes them, so a copy of the log holds its writer's digest. A trading server takes a copy of another
trading server's log when a successor restores it from an archive or a predecessor, when a rotation
member follows the operator, and when a standby takes over. It refuses the copy if an instruction
that a checkpoint covers carries no seal, or a seal by a key that the vault never recorded. An
instruction of that kind after the copy's last checkpoint is refused by a handover's and a
takeover's replay. A member's replica instead keeps those instructions in
`<log_dir>.unaccepted-<seq>`, raises the Critical alert `copy_tail_unaccepted`, and cuts the copy back
to its last checkpoint. The operator's stream then refills the copy.

A rotation member whose copy is empty backfills it from the archives of the operator it follows and,
with `[daily_snapshots]`, starts its replica from the newest snapshot, of those it holds and those the
archives list, whose checkpoint the copy holds and the vault has finalized, and whose state is from
before the turns of the member in office began, the first of the turns in a row that the finalized
schedule draws for that member at trusted time. A member that has held the log before takes none
after the last instruction its account history index took, so its replica passes where its index and
its chart history stopped. It fetches a listed one
whole, checking its length and hash against its listing, and takes a snapshot only when its signature,
which it checks before it decodes the state, covers the state's bytes and is by the key that signed its
checkpoint, a key the vault recorded, and the copy holds that checkpoint at the snapshot's offset. It
tries the three newest in turn, removes a fetched one it refuses, and fetches an archive's copy of a
held one it refuses. A copy that credited a deposit or approved a withdrawal before custody finality is
replayed whole, as for a recovery cache. Before that checkpoint it applies
nothing: it checks every instruction's chained digest and
seal, every checkpoint's signature and sequence number, and every withdrawal authorization, as a whole
replay of the copy does, and computes from the instructions and the events their seals cover what that
replay carries across the checkpoint besides the state: the running digest of every instruction, the
checkpoint accounting, the approvals still to pay in payout order with their latest authorizations,
and who sequenced each range. Its replica then applies the snapshot's checkpoint, computing the root of
the snapshot's state over every key, each market's index of resting orders rebuilt from the accounts'
orders, which must be the checkpoint's root, and replays the copy from there. A thread of its own then
applies the copy before the snapshot to an engine of its own, checking each instruction's events, and
keeps the financing rows of the rollovers there for the member's history index, which a whole replay
would have kept as it went; a turn that comes first leaves the rest to the history index, which
replays the log for them. The marks history it folds starts at the snapshot, so for the markets the
provider backfill does not cover it serves log-derived marks from there until a whole replay at its
next start. A failure of the snapshot or of the copy raises the Warning alert
`snapshot_start_refused`, and the replica replays the whole copy instead, which refuses a faulty copy
for itself. What the member trusts is the state at the
snapshot's checkpoint, on the word of the vault-recorded key that signed that checkpoint and the
snapshot, as the vault and every exit trust that checkpoint's root; a fault of that enclave carries
into the replica as it carries into the vault, and a whole `engine replay` finds it. A member whose copy
holds instructions rebuilds its replica by a whole replay, as every later start does.

Before a restarted trading server writes anything, it reads the posted head through the custody RPC
quorum. It refuses to start on a log that lacks that head, and raises the Critical alert
`log_rollback`, naming the missing range or the sequence number whose digest differs. A start from a
snapshot at or past the posted head hashes the instructions after the head, through the snapshot's
checkpoint, onto the head's digest, and requires the digest the snapshot certifies: a match proves
the snapshot's history holds the head, and none of the log before the head is read. A posted head
that overtakes the running trading server, at a higher sequence number or with another digest at a
sequence number it retains, freezes sequencing.

Known limits:

- `engine replay` checks the seals between a run's first and last only for their form, since the
  chained digest leaves seals out. A log whose signatures there were altered, their signer and
  deployment kept, replays to the same state, events and roots, but its running digest, which reads
  each seal's bytes, differs from its writer's and from any posted log head that covers it. So does
  a log in which the seals of a block of instructions between two runs of one signer were relabelled
  to name that signer, although that signer's last seal chains them. `engine replay --every-seal`
  refuses both, as every replay that checks every seal does.
- These checks reach only as far as the vault's checkpoints and the posted heads. Within that reach,
  whoever runs the trading server can choose after the fact whether its own recent instructions
  happened, by holding a sync or by cutting the log back, as the items below describe.
- **Holding a sync.** The host can hold a batch's sync to disk while it watches the market, then let
  the sync finish or kill the trading server. Until the batch is durable, nobody outside has seen
  it, and a batch that the restarted log lacks never happened.
- Once the oldest unsynced batch has waited 5 seconds, the trading server accepts no further request
  and raises the Critical alert `log_sync_held`. Nothing is stamped behind a held batch, so a hold
  of the log's disk alone is raised within 5 seconds.
- The log's batch syncs also wait behind other writers' buffered data on their disk, and a flush of
  hundreds of megabytes held them for 200 ms to over a second. A production build therefore refuses
  a log directory on the same device as the history index, the marks cache, the recovery caches or
  the daily snapshots. The check reads device ids at load, so it does not see two volumes that share
  one physical drive.
- The alert's webhook page is first written to the alert outbox on the same disk, so a hold of the
  whole disk delays it, and a kill during the hold loses it. Meanwhile the `engine_log_sync_held`
  gauge, served from memory on the metrics listener, reports the hold (`EngineLogSyncHeld`),
  although a host that holds the disk can also answer for that listener.
- **Disk space.** The trading server reports each store's bytes and each data volume's free space, and
  raises `disk_space_low`, `disk_space_critical` and `storage_unreadable` from them. The host reports the
  sizes and its clock times the growth, so a host can show a volume as empty or full. The figures are for
  the operator, who runs the host; no engine rule, checkpoint, client answer or signature depends on them.
  A host that fills or stalls the log's disk is caught by `log_sync_held`, not by these figures.
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
  the head has moved. While a turn names the operator, heads are posted twice per heartbeat interval
  instead, as [liveness and promotion](#liveness-and-promotion) describes. To pass
  unnoticed, a cut must land within about one posting interval, plus the time to include a post on
  chain, of the log's end. A host that stalls or withholds the poster's submissions extends that
  window by however long the tail stays unposted.
- The running trading server adopts an on-chain head as the parent of its next post only when the
  head's digest is the one its own log holds at that sequence number, since a second copy of the key
  signs correctly too. A head at a sequence number for which it retains no digest is not adopted, so
  its posts fail (`log_head_post_failed`) and the head stalls (`log_head_stalled`) until a restart
  checks the posted head against the whole log.
- `log_head_stalled` is raised once the head has not advanced for three posting intervals, or for
  two with `operator.rotation_enabled`. While a turn names the operator, a posting interval is half a
  heartbeat interval, so the alert is raised after one heartbeat interval. The count runs in trusted
  time from a stamp that recovery authenticates, which is the stamp of the head that startup checked
  or, while no signer epoch of this key holds a head, the stamp of the checkpoint the vault has
  finalized. The count starts once the restarted poster's first post has had a posting interval to
  be included.
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
- **Segment boundaries.** A crash can leave the log's newest segment empty, because a segment is
  listed before its first instruction is durable. A checkpoint, a payout authorization or an
  inherited withdrawal's signature must follow an instruction in its segment, so the trading server
  writes none of them until the next instruction is sequenced. A host that times a crash there can
  delay those entries until then. It cannot erase or reorder any of them by doing so, and the
  delayed entries are written afterwards.
- **A standby's check.** A standby, a trading server that follows the operator's signed stream into
  a copy of the log, does compare. It takes an instruction only when its seal continues the copy and
  is the followed enclave's, or one that a checkpoint in the copy names, so history sealed before a
  handover still extends it. Each stream it resumes must first repeat the copy's last instruction
  and every entry after it exactly, with each signature and signer compared as the bytes it holds.
- **A standby's cold start.** The completed segments that a standby backfills from the archive, its
  bucket and its mirrors first, are checked only by hash against the manifest that the answering origin served.
  Before the standby first follows onto them, it replays them as its takeover would replay them. A
  copy that does not replay, or that a mirror cut short of its first checkpoint, is set aside, the
  standby raises the Critical alert `standby_backfill_refused`, and it backfills again from the
  operator alone.
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
  checkpoint. A snapshot also carries each withdrawal approval still unpaid at its checkpoint, with
  the latest authorization the log holds for it, and leaves out those custody's finalized evidence
  showed paid. A start from it checks each authorization logged after the checkpoint against the
  pinned signer and a checkpoint root that the snapshot or the log holds, not against the vault's
  signer-epoch history, which only full replay reads. With snapshots, account history and the
  segment catalog are read and updated only through proofs against roots the trading server signed.
  A snapshot's account history may trail its checkpoint. A start then hashes the log's instructions
  between them onto the history's signed instruction-prefix digest, requires the snapshot's signed
  digest, and refuses the snapshot otherwise; the history index takes those instructions again from
  the log, through the segment catalog's proofs.
  Their signatures establish integrity, not freshness, so the same independent check against custody
  applies, and a full `engine replay` checks the log segments a snapshot skipped.
- A read of the authenticated account history checks each value, and each node along its proof,
  against the signed root. A stored node that no read or update uses is checked by the next update
  that rewrites its parent, so corruption there is found then and not before. The file finds a
  namespace's rows by a small number it keeps beside them. A host that changes that number makes
  reads find other rows, which fail their proofs as any rewritten row does.
- The host can withhold, omit or replace the daily snapshots it serves, and a day on which its
  writer is still busy with the last snapshot has none. A replay in ranges accepts a snapshot only
  at a checkpoint the log holds, so this makes the replay slower, down to a whole replay, and never
  makes it accept a state the log does not produce.
- The alert outbox keeps pages on the host's disk. It checks their schema and rows, but cannot
  detect their deletion or rollback, and its age budget across restarts uses the host clock.
- Without snapshots, the account history index is read from the host's disk with only its schema and
  integrity checked, and its rows are not authenticated. It is matched to the log by one
  instruction, the newest it has taken, and the time its pages give for where their history begins
  must be that instruction's, which a start checks. SQLite reads it in defensive mode, and each event it
  serves names its place in the log, where anyone can check it. An index behind the daily snapshot a start
  went on from, as one rebuilt after its directory was removed, catches up through the host's
  segments before the snapshot, which the start did not check, and derives the financing rows of a
  rollover there from them; the index trusts the host's disk either way.

## The log's archive

With `[archive]`, the trading server keeps the log in a bucket in object storage as well as on its
host, so the log stays public whatever the host keeps. The bucket holds the log in ranges of 10,000
sequence numbers, each compressed with zstd at a key named for its custody chain, its vault and its
first sequence number, so no two logs share a key. A range holds the lines of every entry whose
sequence number it covers, in log order, so every trading server builds the same bytes for it, however its own
segment files are cut; a read-back that spells the same entries otherwise, as a copy backfilled from
another archive may, counts as the same range. The trading server uploads a range
only once the vault has finalized a checkpoint past its last sequence number that its own copy of the
log holds at the vault's root, since nothing at or below that checkpoint is ever rewritten in the
vault's history, and a copy that holds another history after an earlier checkpoint uploads none of
it. It builds a range only from segments checked against the listing its log writer publishes. It counts the range as archived only once a read of its object
from the bucket's public origin, over TLS that ends in the enclave, decompresses to those entries.
Every upload is create-only, and the bucket's lock keeps each object under `log/` from being
replaced or deleted, whoever holds the keys. The trading server writes keys of three kinds: a range's,
worked out from its first sequence number, a tail's piece, below, worked out from its checkpoint's
sequence number and root and its own first sequence number, and a daily snapshot's under
`snapshots/<chain id>/<vault>/`, named by the offset its manifest line gives. Each upload refuses,
before any request, a key that is not a plain file name directly under its own prefix, a tail's being
the directory named for its checkpoint, so no name the host writes leads an upload out of it or onto
another object's key. The bucket is not trusted: a reader replays what it reads, as it does any copy
of the log.

Readers take the log from the bucket's public origin themselves, and the enclave relays none of it.
A listed segment that the host no longer holds is redirected to the bucket's object for the range
that holds its first sequence number. A reader works out each key it needs from the sequence
numbers it needs, and checks each range it reads: the range must decompress within a bound, and it
must hold its own sequence numbers in order, from the first. A segment rebuilt from the ranges is
taken only if its length and SHA-256 equal its manifest line, as for a segment that a trading server or a
mirror serves. A copy built from it then replays before anything relies on it: a standby's and a
member's copy, a successor's copy at a handover, and a replay from the archive's URL. That replay
checks the command seals, the links and the checkpoint signatures by recorded signers, and, with
`--custody-config`, the vault's history. Backfills read the bucket first and fall back to the trading
servers' origins, mirrors first, on any failure. A `404` for a key that a reader needs means the object is
missing, never that the log ends there, since a cache in front of the bucket can answer a stale
`404`: past one, the reader asks the trading servers first and asks the bucket again for a segment that no
trading server serves. A segment that no source serves pages `archive_segment_missing`, and the read fails.

The bucket alone holds the log through the vault's latest checkpoint, which is what the exit needs once
every operator's host is gone. The ranges stop below the vault's finalized checkpoint, and the
vault's latest checkpoint is one of the checkpoints the trading server holds from that one on: a paced
checkpoint is signed only once its parent is final, and a handover's, a shutdown's or a protocol
activation's checkpoint may come between them. For each of those, the
trading server uploads the checkpoint's tail: pieces shaped and named as ranges, under a key named for
the checkpoint's sequence number and root, from the first range the bucket lacks to the checkpoint's
own, whose piece ends with the checkpoint's entry. It finds the checkpoints by reading its log as the
log writer publishes it, the open segment's flushed prefix checked against the hash the writer
publishes, and uploads a tail as soon as its log holds the checkpoint, before the vault has it, each
piece create-only and counted only once a read from the public origin gives back the same entries.
A reader takes the vault's `latestSeq` and `latestRoot`, reads the bucket's ranges until the first it
lacks and the tail's pieces from there, and keeps the lines up to the checkpoint's entry, whose root
must be the vault's; the log it builds then replays, its commands rebuilding that root, before
`engine exit-proof` builds a proof from it. A checkpoint that the vault never takes, as a superseded
copy's, lands under a key that names its own root, which no reader derives from the vault. A tail
that still waits once the vault has finalized a later checkpoint is dropped. A copy
that has stopped at a range that reads back otherwise, or that the vault's history has superseded,
uploads no tail.

Known limits:

- The host can withhold an upload or its read-back, which the trading server then never counts as
  archived, and pages `archive_upload_lagging`; it never makes the trading server wait. The host
  keeps the whole log meanwhile.
- The endpoint, the bucket and the public origin are in the attested configuration, so the host
  cannot point uploads or their read-backs elsewhere. The trading server signs uploads at the host's
  clock, so a skewed clock only makes the bucket refuse them.
- The trading server takes the vault's finalized checkpoint, its sequence number and root, from the
  checkpoint endpoint, which it trusts for the vault's state, as the checkpoint poster does. An
  endpoint that names a checkpoint the vault has not finalized, at the root a trading server's own copy holds,
  can have a range uploaded that a takeover then rewrites; every trading server then stops at that range with
  `archive_object_mismatch`, and none uploads a second version of it.
- A holder of the bucket's keys can create an object at a key the trading server has yet to write.
  The lock then keeps it there: the trading server never counts that range as archived, pages
  `archive_object_mismatch`, uploads nothing past it, and keeps the log on its host; readers refuse
  the object when they replay it. Only the deployment's owner and its members hold the keys.
- A segment file the host altered or removed before its range's upload fails the listing check, and
  the trading server uploads nothing past it until the file is restored.
- The trading server redirects a listed segment that its host no longer holds whether or not its
  ranges are in the bucket yet. A reader then finds the object missing, pages
  `archive_segment_missing` and fails, so a host that drops a segment before its upload denies
  readers that segment but cannot alter it.
- A replay from the archive's URL checks only what the archive lists. Without `--custody-config`,
  an archive cut short, or a trading server that stops listing its last segments, replays to a shorter log;
  with it, a log that ends before the vault's finalized checkpoint fails as truncated.
- A rebuilt segment must spell its entries as its manifest line's bytes do. Where the bucket's
  range was uploaded from a copy that spells them otherwise, as a copy backfilled from another
  archive may, the reader takes the segment from a trading server, and fails where none holds it.
- The bucket holds the log through the vault's latest checkpoint only once that checkpoint's tail is
  up. A host that withholds the upload, or that is gone before it, leaves the bucket short of the
  vault's latest checkpoint until another member uploads the tail from its own copy; if every member's
  host is gone first, the bucket alone never holds the log through that checkpoint, and a reader
  fails, naming the object it lacks. A tail that waits pages
  `archive_upload_lagging` once it has waited `archive.lag_alert`.
- A tail starts at the first range that the bucket lacked when its uploader built it. A reader that
  meets a stale `404` from a cache for a range below that fails, naming both objects, until the
  cache's entry expires.
- A holder of the bucket's keys can create a tail's object first, at a key a reader derives from the
  vault. The lock keeps it there: the trading server pages `archive_object_mismatch` and uploads no
  more of that tail, and a reader's replay refuses it, so the bucket alone no longer serves that
  checkpoint. Only the deployment's owner and its members hold the keys.

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

The scan reads each window of blocks once. A strict majority of the providers must agree on the
window's deposits together with the hash of its last block, which each provider reads before its log
read. They must also agree on the vault's deposit count in that block, read by its hash, and on the
header of every block a deposit is in, whose hash must be the deposit's. The window's last block
becomes the boundary.

Each poll reads the header of the boundary it started from again and compares its hash with the one
stored. Each provider reads that header after its own log read for the poll's window, and the quorum
votes on the window and the boundary as one answer, so no majority can join a window from one branch to
a boundary from another. A window joins the scan only once its boundary holds. A block's hash commits to
its parent's, and so to every block below it. A reorganisation at or below the boundary therefore
changes the boundary's hash. One above the boundary changes no block the scan had read. On a mismatch,
the scan walks back through the stored window ends, halving the stretch each time, to the highest end
that still matches, and reads again from there. A walk that a failed read stops sends the scan back to
the finalised frontier instead, so a mismatch once seen is not forgotten. Reconciliation then moves or
reverses whatever the new chain does not hold. A reorganisation deeper than every stored end sends the scan back
to the finalised frontier. That frontier's header is read each poll, and a change to it is refused as
inconsistent. One boundary read a poll therefore detects every reorganisation of blocks the scan has
read that is still in place at the poll. Reading every block's header found the same reorganisations,
and a block with no deposit changes no state.

What the boundary read does not catch is a strict majority of providers that each answer one poll's
window from a new branch and the boundary, read after it in the same answer, from the branch the scan
holds, as backends behind a load balancer can during a reorganisation. The stored window ends then come
from two branches, and the next boundary read matches only the newer one.

Principal is therefore released only against the block the release tag names, or against the scan's
end when the headers above the end, read through the quorum, link it to that block. The trading
server executes its reads in that block by its hash, so a scan that differs from the chain the tag
commits to is never released. The finalised frontier moves only to that block, once the scan covers
it. Before it moves, the trading server reads, in that block, the vault's deposit count and its
record of every deposit it releases: the payer, amount and beneficiary. It also reads the header of
every block such a deposit is in. On any difference, it scans again from the finalised frontier. A
deposit credited from a window that straddled a reorganisation, or from providers answering from a
branch the release tag has left, is therefore reversed or moved, as after any reorganisation before
finality, at the latest once the release tag passes its block, and never released.

When the quorum proves that a deposit was reversed before finality, the engine debits it once,
raises a Critical alert, and runs the usual close-out, writing off against the house any loss on an
account left with no positions. Positions without a price wait for one, and a negative house net
asset value does not halt trading. A deposit is treated as reversed only when the chain has dropped
it: an orphaned deposit is debited only if no replacement appears up to the eligible head.

A deposit that a reorganisation includes again in another block, with the same beneficiary and
amount, moves to its replacement receipt, even at a changed log index or a changed vault deposit id.
A reorganisation that includes another deposit ahead of credited ones shifts each of them to a later
id. The trading server therefore applies all of one reorganisation's moves in a single command, in
which every replaced receipt leaves its id before any replacement takes one. No deposit that is still
on the chain is debited or closed out for holding an id that another deposit now has, and the engine
refuses any credit or move that would land on an id a live receipt holds, or above the finalised
frontier on an id that the frontier counts, which belongs to a deposit at or below it. Each moved
deposit's cash nets out, and a Critical alert is still raised for each replaced receipt. The trading
server sequences none of a reorganisation's changes until each orphaned deposit is either paired with
its replacement or, once the scan reaches the eligible head, known to have been dropped.

The chain can drop a deposit at the eligible head and include the same transaction again a few
blocks later. In that case the reversal may have left the client's account below zero, and the house
may have written that shortfall off, at once or after a market that could not trade reopened. The
engine keeps each dropped deposit against its account and records every later write-off against that
account. When the same deposit, with the same beneficiary and amount, is credited again, the engine
repays the house, from its principal, the write-offs recorded since it was dropped that no other
returning deposit has repaid. The client keeps no loss that the house absorbed while the deposit was
missing, pays nothing for a write-off from before it was dropped, and repays each write-off once.

A reversal opens a signing hold, under which no state root is signed until the next finalised custody
frontier. The trading server sequences that frontier only after every change of the scan, so the
deposits in a signed root always match the chain at that frontier. Any intermediate state between a
reorganisation's first change and the next frontier is never committed to a signed root.

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

- While a daily turn is in effect whose set names two or more members, the enclave also signs a
  withdrawal only once a check by another member covers its approval, as
  [the log check before withdrawals](#the-log-check-before-withdrawals) describes.

## The vault's governance

The deployment's multisig proposes changes through a timelock with a nonzero delay, which is three
days at launch. The timelock lifts a withdrawal pause, changes the deposit pause, and names the
release that may hold the enclave's keys. While the registry's `rotationEnabled()` is false, the
timelock sets one launch measurement and configuration hash as the current release. Nothing else
changes that release then. `rotationEnabled()` is false while rotation's switch is off and no drawn
turn names the operator. Turning rotation off leaves the vault naming the release its keys are on.
Once an installation ends the drawn turns, the vault names no release until the timelock names one.
While rotation is on, the timelock approves releases for the operator in effect, whose candidate
account moves the vault's current release to one of them, withdrawing the approval of the release it
leaves. A release belongs to the first candidate that the timelock approves it for or installs on
it. The deployment's release belongs to the deployment's operator. The registry refuses a release
for any candidate but its own.

The timelock installs the vault's signer. While rotation is on, it can install only the enclave
address that the operator in effect registered. The vault also moves its signer, with no timelock,
to a member's vouched standby key when the schedule promotes that member, as
[liveness and promotion](#liveness-and-promotion) describes.

The timelock also names the vault that `MonkeyMinter` mints against, which must be a vault under the
same timelock, and the vault and the schedule that the registry follows. It moves the minter to
another vault only once the guardian has retired the current vault's signer, so that vault's roots
claim nothing more. It changes the registry's vault and schedule, which must be built for the
registry over log heads that follow that vault, only while rotation is off and no drawn turn runs,
of the schedule it follows or of the one it names, which may have drawn while the registry followed
another, and, but to name a first schedule for the vault the registry follows, only once the guardian
has retired that vault's signer. A vault reads its registry's schedule, so after either change it reads
one that drew none of its turns, and names the release it held before its own turns again. The vault promotes a signer, and hands out a turn's
generation, only for the schedule the registry names, so a schedule the registry has left cannot act
on the vault it follows. Log heads follow one vault for good, so those of a vault whose signer is
retired take no head.

The vault's guardian is fixed at deployment, and the deployment sets it to the same multisig, which
acts as guardian without the delay. The guardian may pause withdrawals and USDC distribution claims
for seven days, and it cannot extend that pause. It may also retire the signer. After a retirement,
the vault refuses withdrawals, checkpoints and promotions until the timelock installs a signer. Its
roots prove no MONKEY or USDC claim until that signer posts a checkpoint, which an exit that starts
first rules out.

Known limits:

- The vault trusts the timelock's choice of signer. While rotation is off, the timelock can install
  any signer. While rotation is on, it can install the enclave address that the operator in effect
  registered, which the chain does not check against an attestation.
- The trading server learns of a pause or a retirement from finalized evidence. For up to one
  finality delay after either, it can still sign authorizations, which the vault refuses.
- The minter trusts the timelock's choice of vault. Once the guardian has retired the current
  vault's signer, the timelock can point the minter at a contract that answers forged roots, and mint
  up to the cap. While rotation is off, its `setSigner` already lets it install a key that signs
  such a root on the vault itself, and turning rotation off takes the timelock's delay.
- The registry trusts the timelock's choice of vault and schedule. While rotation is off and no drawn
  turn runs, the timelock can pair the vault with a schedule that promotes any key, which `setSigner`
  under the same condition already allows. A vault the registry has left, or whose schedule it has
  changed, names the release it held before its turns, in that release's generation, so a sealed copy
  of its keys from then opens again; its signer is retired by then, so no key it opens signs for that
  vault.
- The registry cannot move off a vault that does not answer `checkpointSigner()`, so it refuses to
  name one. It cannot tell a vault from another contract that answers it.

## MONKEY staking and distributions

`MonkeyMinter` mints each account its MONKEY emissions, and the vault pays each staker its USDC
distributions, against entitlements proven against the root of any recorded checkpoint of the
signer epoch in force, less what the account has already claimed. The minter proves against the
roots of the vault it follows, and counts what it has minted each account across every vault it has
followed, so an entitlement a later vault's log carries mints only what was not minted before. The staking contract holds staked MONKEY, enforces the unstake
cooldown, and emits the events that the engine mirrors from finalised blocks. The trading server
mirrors only the staking contract that the configured registry records as the one it deployed, and
checks `InitStaking`'s epoch and cooldown against that contract alone, so a configuration cannot name
a contract that merely answers as the registry's own. The staking mirror's references to those
events, and the anchors that record how far it has scanned, are part of the state root. The
mirror's frontier is the latest finalised block it has mirrored. A distribution epoch closes only
once that frontier has passed the epoch's end, so no epoch's close pays a stake set that the mirror
has not brought up to the epoch's end.

The mirror reads a window of finalised blocks in one log read over its range. A strict majority of
the custody providers must agree on it together with their header at the window's end. A window that
ends at the block the release tag names must end on the hash the quorum voted with the tag. One that
ends below it, in a catch-up, is read with each provider's header at the release tag's block in the same
voted answer, which must hold the tag's hash, so either way the window is bound to the release tag's
chain. The frontier the mirror records is read again each poll. The headers from the lowest block an
event is in up to the window's end are read through the quorum and must each name the one below as its
parent, ending on the window's end, and each event must be on its block there. Every event block is
therefore on the release tag's chain, whatever backends answered. Each such block is also read again by
its hash, and the mirror stops if those reads disagree with the range read. A provider whose log backend
holds another fork therefore cannot add an event. A log backend that lags cannot drop one from a block
where it returns another.

Epochs close in turn. When the mirror falls behind and then catches up, each epoch whose end it has
passed closes in order, at the stake set the mirror held when it passed that end, and no epoch is
skipped. A holder that stakes after the mirror's first completed scan past an epoch's end is
therefore paid nothing for that epoch. The
operator's share of an epoch is split among operators by their sequenced time in office up to the
epoch's end, however late the close runs, and the next epoch counts time in office from that end.
An operator that holds back the mirror therefore gains no share by doing so.

Bug bounty awards are paid from the same distributions. Only a passed vote of the rotation set
approves an award. The engine refuses the command that approves one, or that moves the awards' cap,
when the operator signs it or the trading server sends it, in every state, and the fast path refuses
both. An award's amount must be the scale's amount for its severity: 250,000 USDC for Critical,
50,000 for High, 10,000 for Medium and 1,000 for Low. Each close that distributes pays the queue of
awards first, oldest first, up to the lesser of 5% of its distributable excess and 50,000 USDC. A
vote may move that cap to between 5% and 20% and between 50,000 and 250,000 USDC. An award that a
close does not finish stays at the front of the queue. An advisory is awarded once. The engine keeps
each awarded advisory identifier in its state, and refuses a second award that names one, in any
letter case, when its vote applies. The operator's fifth and the stakers' shares come from what the
awards leave, so the stakers and the operator keep at least 80% of every close. Each payment is
added to the recipient's cumulative distribution, which the vault pays as it pays a staker's. In the
vault's exit, the part of an award already credited ranks with the stakers' distributions, and the
part still in the queue when the exit starts is lost.

Known limits:

- The staking contract keeps each staker's delegations as amounts, with no limit on how many
  candidates one staker delegates to. An unstake larger than the staker's undelegated stake reduces
  every delegation in one transaction, so a staker with very many delegations must first take some
  back, one or two per call, before such an unstake fits in a block.
- That cost falls only on the staker that chose the delegations, and no one else can add to them. A
  report that rests on it alone is not a new finding.
- The host can delay a distribution by holding back the mirror. The excess that a delayed close
  pays is the amount the house holds above its floor when the close runs, so it includes what the
  house earned during the delay, and it is paid to the stake set of the epoch that closes. The stake
  set of a delayed close also includes the stakes that the mirror's first scan past the epoch's end
  delivered, as it does for a prompt close.
- The operator's share of each distribution is split among operators by their sequenced time in
  office. An incumbent whose host delays an elected successor's takeover therefore keeps earning the
  share through that delay.
- In a log from before staking mirror references entered the state root, recovery that can write to
  the log needs authenticated origin seals on the older staking instructions.
- The engine cannot check that an award's advisory is real and published, that its severity is
  right, or that two advisories do not describe one finding. The voters judge those, and the vote is
  only as independent as the stake behind it.
- A proposal opens only while rotation is on and a rotation set is in effect, and only a member of
  that set proposes it. It then takes votes from that set's members, at the weights the set adopted,
  until its window closes, while rotation is on, and it passes when the yes weight first passes two
  thirds. It can therefore pass after a later set with no members has taken effect, and the engine
  records the pass while rotation is on and any set, with members or none, is in effect. A vote that
  passed applies seven days after it is logged, even if rotation has turned off in the meantime, so
  an award can be approved, or the cap move, while rotation is off. The queue is paid whether or not
  rotation is on.
- An award is paid only by closes that distribute. While the house's net asset value is at or below
  the liquidity floor, or nobody is staked, the queue waits.
- The quorum alone stands behind a window's range read for a block from which it returns no event. If
  a strict majority of providers have log backends that lag behind their own header at the window's
  end, and all return the same range without some block's every event, the mirror passes that block.
  Reading every block's header, whose logs bloom flags any mirrored event, would catch this, at one
  header read per Arbitrum block.

## The move to a new vault

A deployment can move to a new vault and start a new log whose genesis carries, from the old log's
final checkpoint, each account's MONKEY entitlement and staked MONKEY, the point to which the old log
had mirrored the staking contract, the emission schedule with its totals, and every bug bounty award.
Only the trading server sequences a carried genesis, in its genesis, after staking is initialised
and before anything is credited, staked, entitled or awarded. Distributions, balances, positions,
orders, approvals and deposits are not carried, and each carried award owes its whole amount.

The engine proves every carried value against the old log's checkpoint root, which the genesis
names, so a host or a configuration author cannot alter a value. The old log's remaining state,
proven against the same root, must record the old vault and its chain as its client
authentication, the carried sequence number as its last instruction, protocol V90 or later, and
rotation's switch off. The engine checks that the carried sets are complete. The entitlements must
sum to the old log's total entitled, the stakes must sum to its total staked, and the awards must
run from id 0 to an id at which the root proves no award. Because every value is at least zero and
every account is carried at most once, no account with a positive entitlement or stake can be left
out. A part that fails any check is refused, and the trading server refuses to serve a log whose
genesis holds a refused part.

At its first start, before its genesis, the trading server reads the chain at a `finalized` block
through the custody providers. It carries only from the old vault's latest checkpoint, and only once
that vault's signer is retired, the registry follows the new vault, rotation's switch is off, the
registry's operator in effect has the configured operator key, and `MonkeyMinter` follows the new
vault. The configured registry must be the one the old vault reads, the configured staking contract
must be the one that registry records as the one it deployed, whose events the old log mirrored, and
`MonkeyMinter` must mint the token that contract holds. A configuration author therefore cannot carry
from an older root of the old vault, nor continue the carried stakes on another staking contract. The
carried emission budget must match the cap that `MonkeyMinter`'s token holds.

`MonkeyMinter` keeps one count of the MONKEY each account has claimed across every vault it has
followed, so an account can claim from the new vault only what its carried entitlement adds to what
it has claimed already. No emission period before the new log's first instruction after its genesis
allocates MONKEY. The first instruction past the carried period closes that period with no weight,
and cancels every later period up to its own.

Replaying the new log alone verifies that every carried value is in the logged root, that the root
is the state after the logged sequence number of a log whose custody was the logged vault, and that
the carried sets are complete. `engine replay --carry-source` also replays the old log to the
carried sequence number, requires the old log's signed checkpoint there to hold the carried root,
and requires the carried state to equal the old log's state there.

Known limits:

- A replay cannot tell that the carried root is the old vault's final accepted checkpoint. That is a
  fact about the chain, which the trading server reads at its first start. `--carry-source` ties the
  root to the old log's signed checkpoint at the carried sequence number, and a verifier compares it
  with the old vault's `checkpointRoots` there and its `latestSeq`.
- The old log must stay published for good. Without it, a replayer can check the carried values
  against the logged root, but cannot tie that root to the old log's history.
- A carry loses the old log's open emission period. The tooling refuses an old log whose open period
  has results, and the old log is frozen before its final checkpoint, but the engine cannot check
  that the period was empty, because the period's results are not carried.
- A carry loses any award vote that passed in the old log and has not applied, any distribution the
  old vault has not paid, and the old log's cap on award payments. The old vault's exit pays its
  unpaid distributions in its own currency.
- From the old log's last drawn turn until a rotation set adopted after the move has waited out its
  seven-day notice in the new log, no standby can be promoted. Only the timelock's install fallback
  can replace a failed operator in that time.
- The old log's schedule and vote contract stay on chain. Once rotation is on, anyone can draw turns
  on that schedule, and its rotation set can vote there. The new log mirrors neither.

## The enclave and its keys

The trading server runs inside an SEV-SNP enclave built from a reproducible image. Its attestation
report binds the enclave's signing address, the SHA-256 hash of its configuration file and the hash
of its TLS key. `engine verify-attestation`, and the browser build of the same code, check a
report's certificate chain, measurement, policy, CPU model, firmware versions and configuration
against a release manifest.

AMD numbers each firmware and microcode security version per CPU model, so a release manifest lists
each CPU model it accepts, with the lowest versions it takes from that model, and a report's
versions are compared with its own model's only. A report names its model three ways, and all three
must agree with one entry: its CPUID family, model and stepping, which reports carry from version
3, so a version 2 report is refused; its VCEK's product name; and the product line that the AMD
root and intermediate above that VCEK are named for. `--processor` and `tee.processor` must name
that product line too. The launch release accepts Siena (EPYC 8004) alone, and accepting another
model takes a new release, with that model's own minimum.

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
the other over a challenge fresh to the connection. An enclave takes a peer's quote, in a handover,
a standby key's vouch or a log check, only from a CPU model that its attested configuration's
`[[tee.cpu_models]]` lists, the release manifest's entries, at that model's own minimum, never at
its own platform's versions. A trading server whose own processor its list does not admit raises
the Critical `cpu_model_unlisted`. The operator's tool holds an enclave receiving secrets to the release
manifest's entries the same way. The keys travel encrypted to the new release's
attested one-time key, and the new release takes them only if the vault already names their signing
key. A release that has stopped in order to hand over its keys serves new releases until one
confirms. A trading server that the vault does not name as signer signs nothing and serves only its
attestation. The handover endpoint is public, so the holder sends each connection its challenge at
once and serves the requests one at a time. One address holds at most two of its 64 connections. A
connection that sends no request within five seconds of its challenge is dropped. With every place
taken, or both of the new connection's address, the oldest connection there that has not sent its
request gives its place up to the new one. The new release fetches its quote's vendor collateral
before it connects, so its request follows the challenge by one quote. The delivery listener holds
connections the same way, and quotes for at most eight of them at once.

The trading server's credentials reach it only encrypted to a one-time key whose quote proves that
an enclave of the release holds it. They are its RPC provider keys, its market-data vendor keys and
key pairs, its screening source key, the seed relayer's Ethereum provider and beacon
node URLs, and its alert webhook URLs. The operator's tool checks the quote against the release
manifest before it encrypts the bundle, and the bundle must carry the signature of the operator key
in effect. The attested configuration names which secret each field reads, and fixes each RPC
provider's origin, so a bundle, even one signed by a stolen operator key, can change a provider's
key but never which provider credits deposits. Secrets are sealed per release, a recorded delivery
does not decrypt on a later connection, and an election moves none of them.

Known limits:

- The SEV-SNP sealing key, the hardware quotes exchanged in a handover and the image's Secure TSC
  launch are built to the firmware's guest interface, but have not yet run on SEV-SNP hardware.
- A release sets its firmware floor only as a minimum of the reported firmware versions for each
  CPU model. The verifier reads a report's mitigation vectors, which record the vulnerability fixes
  the host has had the firmware verify, but requires none, so a fix that needs that host step rather
  than a newer firmware version is not enforced.
- Each CPU model's floor is only as current as the release that sets it. A fix that AMD ships after
  the release, with a higher version for the model, is enforced only by a new release with a higher
  floor. An enclave holds a peer to that floor, not to its own platform's versions, so a peer of
  the same model whose firmware is behind the holder's, but at or above the floor, is admitted.
- The configuration and the release manifest carry the CPU model table separately: the manifest
  cannot name the configuration it lists the hash of. If they differ, a verifier and an enclave
  take quotes from different CPU models or at different floors, so the release process keeps them
  equal.
- AMD has not published the layout of version 4 of the attestation report, and the verifier reads
  it with version 3's layout, which is an assumption. If version 4 placed bytes that the host or
  guest chooses (`HOST_DATA`, `REPORT_DATA`, or the ID block's `FAMILY_ID` and `IMAGE_ID`) at an
  offset the verifier reads, such as the measurement, whoever chose them could satisfy the
  comparison with the release manifest. The verifier does not check this: the mitigation vectors
  are read without comparison, so not every field it reads must match the VCEK's certificate, the
  release manifest or the served bundle. The assumption rests on two facts: public parsers read the
  version-4 reports that SEV firmware 1.55.31 issues with version 3's layout, and every published
  revision has added fields only in space that version 3 reserved.
- With the mock enclave, the stand-in for the sealing key is derived from public values, so anyone
  can compute it.
- Two copies of the current release that the host runs from copies of one disk can both open the
  keys. The vault accepts one copy's checkpoint and the other copy stops on the conflict, but until
  it stops, its signed API answers still verify.
- A release taking over the keys checks the running release's certificates against the host's clock.
  The vault's naming of the key bounds this.
- Peers on many addresses that open new connections faster than a new release quotes can still keep
  it from the keys, and from a delivery, for as long as they keep it up. Where the host's forwarding
  hides peers' addresses, so that the guest sees them all as one, a single peer can.
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
  server takes bundles only from its own configured key, and under daily turns, until an installation
  ends them, whether or not rotation is still on.
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
- `engine ct-watch` learns a key only from a verified attestation of an enclave that its
  `node.enclaves` names, the vault's signer and the standby keys that a promotion can make the
  signer. A new signer, or a promotion to a key the list leaves out, raises `attestation_unverified`
  until the list names it, and its certificates raise `unattested_certificate` past the grace period.
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
the log from the incumbent's enclave from the election's effective time. The registry elects again
only once the last election has taken effect, so an election that the engine mirrors while the one
before it still waits for the mirror to pass its effective time takes effect after that one: the
earlier winner's key signs from its own effective time until the later winner's, however many a
catch-up mirrors at once. While rotation is on and an election hands the key over or names the key in
effect, the node takes secret deliveries from its own operator's key only, whichever election is waiting,
and its handover alerts follow the first election that names another operator.

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

While a turn is in effect whose set names two or more members, the enclave signs a withdrawal only
for an approval at or below the head of the log that the last check verified. Turning
rotation off leaves the drawn turns in effect, as the vault's `rotationEnabled()` counts them, so they
still hand the keys on at their boundaries and the check applies until an installation ends them. A
check is another member's verification of the log through a checkpoint:

- At a handover, the check is the successor's own replay of the log through the checkpoint it was
  handed.
- At a promotion, the check is the promoted member's replay through the vault's checkpoint that the
  promotion fixed, unless that member sequenced part of the range, as after its own upgrade.
- While a member holds a turn, the check is an attested verification of a checkpoint, bound to the
  turn's boundary, its assignment and the vault's signer epoch: at a boundary that the holder holds on
  through, of the first checkpoint after its `TurnTaken`, and within the turn, of the latest checkpoint
  written once one covers the earliest approval that waits past the verified head. The holder's enclave
  asks the members of the turn's order, first to last, in questions it signs, and a member answers
  only from the checkpoints it verified under that signer. These are checkpoints that the trading
  server writes anyway, so a turn with no withdrawals asks for no check past its boundary's.

A member may answer only if it sequenced none of the instructions since the last verified head,
under any release, and runs no release that did. The log's `TurnTaken` instructions, the promotions
and skips that the schedule records, and the keys' own record of each release that handed them on
past that head decide this. A member promoted or skipped to after the assignment in office, a member
with no handover endpoint, and the holder's own candidate are not asked.

An approval made during a turn therefore waits for the check of a checkpoint that covers it, the
latest written when its check comes due. That check usually lands while the checkpoint is on its way
to finality, so with members answering, a withdrawal is signed as soon as the vault holds its
checkpoint, as without the check. Payouts stay anchored to the roots that the vault accepted, and a
member of the set cannot be paid on a branch that no other member holds. The verified head is sealed
with the keys after every check while the check applies, so a handover carries the latest one, a
host that restores an older copy only delays payouts, and with none the next check covers the
holding of the latest `TurnTaken`.

Known limits:

- A skip that a reorganisation removed is never mirrored. A promotion's standby keys, and an older
  copy of the keys that a host restores, record only the handovers that they themselves saw.
- A check binds a release, not a host. A host that also runs another member's release can feed that
  enclave the branch it withholds, and have it verified.
- The rule bounds neither delay nor censorship. A host that keeps its trading server from asking, or
  members that do not answer, hold payouts until one answers or until the next handover, and
  `log_check_pending` is raised after 30 minutes. A check stays on its checkpoint until a member
  verifies it or the turn changes, so approvals after that checkpoint wait for that check and then
  for their own.
- A boundary whose turn nobody drew logs no `TurnTaken`, so approvals wait for the next drawn turn.
- A start from a snapshot past the verified head cannot say who sequenced the range. It asks no one,
  and raises `log_check_unattributable` until a full replay or a handover.
- A trading server's start, and a member's replica, take a daily snapshot only of a checkpoint that
  the log on the host holds, at or before the vault's finalised one, and, where the vault accepted a
  checkpoint at the snapshot's sequence number, of the root that the vault accepted there. A snapshot
  of a checkpoint that the vault did not accept rests on the vault's later checkpoint, which its replay
  must reach. The state and the replay state at the snapshot's checkpoint are the signer's word: a
  signer that signed a false one is caught by any replay that passes the checkpoint, in ranges or
  whole, not by the start.
- The host can withhold the newer snapshots, so a start goes on from an older one and replays more of
  the log, or remove the log's segments before the newest usable snapshot's checkpoint, after which a
  start with no usable snapshot, or with a posted head older than the links the snapshot carries, cannot
  read the log it needs and refuses to serve, as does a rebuild of the account history index. A start
  never takes a snapshot the vault has not finalized.
- A trading server or a member that started from a daily snapshot did not replay the log before the
  snapshot's checkpoint, so its own check at a handover or a promotion counts only for a range that
  starts after that checkpoint, and a member answers another's question only for a range that starts
  after it, which the question names under the asking enclave's signature. A member starts from a
  snapshot before the turns of the member in office, where the last check's range starts while each
  check counts, so its own check at its first take counts; after a check that did not count, the range
  may start earlier, and with two members in the set payouts then wait until the set changes, as after
  any check that does not count. A trading server set up
  to take no turns signs no withdrawal while the check applies, and raises `log_check_unavailable`.
- Instructions other than withdrawals stay exposed to a branch that the host withholds, for at most
  one heartbeat interval past the newest sequence number that another member confirmed.
- With a one-member set, or with no turn in effect, as once an installation ends the turns, only the
  posted roots gate payouts.
- After rotation is turned off, no turn is drawn past those already drawn. The holder of the last
  drawn turn keeps asking for checks within it. An approval that a trading server holds while it does
  not hold that turn, as before the log mirrors a promotion into it, waits for the installation that
  ends the turns, and is then signed with no check of its range. `log_check_awaiting_installation`
  pages while one waits.

## Liveness and promotion

The schedule records each turn's assignment, which names the member that holds the turn, its
position in the turn's order and the generation in which it holds it. Each member's enclave also
keeps a standby key, which another enclave vouches for and the member's candidate account registers,
so that the vault can make that key its signer in a promotion. Block time on Arbitrum is divided
into slots of one heartbeat interval, and anyone may mark a slot as witnessed while it runs.

**Heartbeats.** While a turn names the operator, the operator's enclave posts an
`engine.log-head.v2` head as its heartbeat, normally twice per heartbeat interval. The trading
server posts a head only after one of its reads of the chain. The reads come at least six times per
interval, spaced so that a whole number of them makes half an interval. A read or a send slow
enough to pass the time a post is due delays that post to the first read after it finishes. One
failed or late head leaves at most one slot without a head, and a promotion needs
`heartbeatMisses` such slots in a row, at least three under any rule the schedule accepts. A turn's
first head under its assignment must land within the take window, at least two slots, and one failed
first head still leaves the next in time. At the first turn after none named the operator, the
trading server reads the chain every second from the moment its latest trusted time sample allows
that true time has reached the turn's boundary, on a monotonic clock running up to 500 parts per
million slow or fast and whatever the host clock reads, until true time has certainly passed the boundary
by 30 seconds or the turn's first head has been sent or its send has failed. The pace of a head it posted before the turn
does not hold that head back: it goes out at the first of those reads after the chain names the turn
and a member has confirmed the log, once any send the relayer still has pending from before the
turn has landed, as the next paragraph bounds it. Under the tightest rule at 49-second
slots, that is within half an interval of the boundary unless the agreed block and the confirmation
together trail the boundary by about 23 seconds. A first head 30 seconds and a read after the
boundary that then failed would, under an interval shorter than 90 seconds with a tolerance near
the interval, reach the chain in time only through the members' relays.

When the chain's newest head is older than five sixths of an interval, each member relays the newest
head that the operator signed. A head that a member's stream brings after that point is relayed at
once rather than at the member's next read, which could come after the head's signing time has left
the head tolerance. The operator's own heads are due every half interval, so members relay a head
only when the operator's posts fail or are slow to land. A send that the relayer's endpoint accepts
but that never lands holds the operator's later posts for one heartbeat interval from the send, and
up to one and a half intervals until the next post due after that replaces it at its nonce. Through
that time the operator still signs its heads, and the members' relays carry them.

The schedule's deployment sets the heartbeat interval, the number of missed slots before a
promotion (`heartbeatMisses`) and the take window (`takeWindow`), within bounds that the contract
enforces. At launch the interval is 140 seconds, a live head's signing time may differ from its
block's by up to 40 seconds, a promotion needs three missed slots, and the take window is two
slots. The deployment fixes the bounds that the timelock can later move these values within: 90 to
140 seconds for the interval, 30 to 60 seconds for the tolerance, three to five slots for the
misses, and two to four slots for the take window, under a switch budget of 20 seconds.

While the turn's set has another member, the enclave signs a head or a checkpoint only at a sequence
number that another member's enclave confirmed holding. The operator therefore runs, and posts
checkpoints, only while another member confirms its log. It stops accepting instructions once its
log runs more than one interval past the newest confirmation. A checkpoint is the log entry after
its sequence number, so the operator stamps nothing past a checkpoint until another member confirms
that sequence number. For a member current with the log, the pause lasts about one stream delivery,
one sync and one round trip. A checkpoint taken at the regular pace holds stamps for at most 250 ms:
if no member confirms its sequence number by then, it is not signed, stamping goes on, and the next
one covers that sequence number. So members that trail the log, as they do while they move to follow
a new operator, cost no prices. Once such checkpoints have been skipped for 30 seconds, the next one
waits for a member however long it takes, under Warning `checkpoint_deferred`. A handover's final
checkpoint always waits.

**Promotion.** Once `heartbeatMisses` witnessed slots pass with no head of the signer epoch, anyone
may promote the next qualified member in the turn's order. The vault then moves its signer to that
member's vouched standby key, in a new signer epoch, with no timelock. Detection takes
`heartbeatMisses` to `heartbeatMisses + 1` intervals after the last head that landed, 7 to 9.3
minutes at the launch values, and at most 10 minutes under the deployment bounds. Each promotion
adds the promoted member's start, and each member in the order that fails to start adds one more
detection window.

A promotion names the first member in the order that has a promotable standby key and never held
the keys of the turn, whether or not it was drawn or skipped to, and it names the key holder last.
Whether a drawn or skipped-to member received the keys is the key holder's choice, so the holder's
refusals never move one member ahead of another. The search runs from the current position through
every member of the set, so a promotion is refused only when no member qualifies.

The key holder is the candidate that owns the release which signed the signer epoch's newest head.
The registry binds each release to one candidate for good. The deployment's release belongs to the
deployment's operator, and any other release belongs to the first candidate that the timelock
approves it for or installs on it. A release that its candidate has moved off therefore still names
that candidate, and no other candidate can take the release.
A head counts only if its enclave signed it since the latest turn drawn while the registry
named its own operator began: the first turn, or the first after an installation. Without one, no
member has taken the keys in the epoch since then, so they are where the epoch found them. They are
with the member of the latest turn that a member took or a promotion reached; in a turn the epoch
began in, with the member whose registered enclave the timelock installed; or else with the source
of the earliest turn since, such as the operator in office when turns begin or after an
installation, whether or not the installed operator took the outgoing operator's keys. An
installation that keeps the signer epoch therefore assumes the outgoing operator handed the
installed one its keys; when it has not, the timelock installs a new signer. A skip that passes a
member holding the keys records it as having held them and as the epoch's key holder, so no
promotion names it in that turn, the next turn's skips pass it, and the next turn's promotions take
it last. A holder that withholds the keys therefore cannot take the turn itself, and cannot choose
whom a promotion names. It can still hand the keys to the member that a skip names next in the
order, which the seed fixed. At the launch values, one skip falls due before an untaken turn's
promotion, because a second would need another two-slot window past the first.

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
successor's reads of the turn's latest assignment after the stop. Each read takes ten round trips
to the custody providers in turn, or eleven while a provider's tip is past the agreed block.
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
that mirror, so the log check never counts the member's instructions as another member's
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
the governance fallback applies. The timelock turns rotation off. Once the drawn turns have ended,
one timelock batch installs an operator with `installOperator` and then names its release with
`setCurrentRelease`. The last drawn turn's member stays named until the installation, so the vault
names that member's release until the batch and the fallback's from it, and never the release it
named before rotation turned on. The fallback takes at most the 10-minute
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
  turn ends. A start holds for the benchmark worker's snapshot only when it finds a rollover unpaid,
  or when its next rollover falls before the worker's next snapshot.
- A held head names the newest sequence number that another member confirmed, not the log's tip. A
  trusted-time halt stops the signed stream, so a tip that no member had received by then could
  never be confirmed, and requiring it would promote a member through every such halt.
- A held head can therefore trail the log's tip by the instructions that no member received before
  the hold, at most one interval's worth. Those instructions stay exposed as any unconfirmed
  instruction is, and a hold stamps nothing past them.
- A standby key's registration is sent by the candidate's own account, so a candidate that does not
  send it is not promotable. The operator nonetheless counts that member's confirmations on the key
  once its enclave has verified the key's attestation.
- A newest head whose release belongs to no candidate leaves the key holder unknown. Skips and
  promotions are then refused with `KeyHolderUnknown`, which a member's liveness caller pages as
  `liveness_key_holder_unknown`, until a member takes the keys, or until the
  timelock approves that release for the candidate whose enclave runs it. Only the timelock can cause
  this, by naming on the vault, while rotation is off, a release it never approved or installed for a
  candidate, whose enclave then keeps the keys into the turns.

## Fault proofs

Anyone can submit to `FaultProof` two `engine.log-head.v2` heads, signed under one signer epoch and
binding the same turn, position and release, that name different digests at one sequence number.
`FaultProof` checks both against that epoch's signer and the schedule's assignment, once per
assignment. It also requires that the assignment held office under that epoch: `LogHeads` must have
accepted a head of the epoch binding the assignment while it was the turn's current one. A key of
an epoch that ended before the assignment took office, or began after it left, therefore names no
one. The engine then cuts the assigned member's operator share by a haircut that starts at the
recorded penalty and falls linearly to nothing over its decay period. The withheld part goes to
stakers.

Known limits:

- Every member that holds office within one signer epoch signs with that epoch's key, which
  handovers pass from member to member. Whoever holds a copy of the key while its epoch is in force
  can sign two histories for any assignment that held office under the epoch, and name its member.
  This needs a key outside an honest enclave, and that key can already sign checkpoints and
  withdrawals.
- An epoch under which no head binding the assignment reached `LogHeads` while the assignment was
  current proves nothing for it, even if the member signed two histories under that epoch. While none
  of the member's heads land, the schedule skips its turn or promotes past it within a few heartbeat
  intervals.
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

