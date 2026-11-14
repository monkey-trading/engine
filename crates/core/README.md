# engine-core

## Overview

The deterministic state machine at the heart of the house. It applies sequenced commands (orders,
venue prices, deposits, withdrawals, operator actions, and time ticks) and returns events. It
does no IO and uses no clocks, randomness, or floats, so anyone can replay the public log against
this crate and reproduce the signed state roots exactly.

## Features

- House as counterparty to every trade, with per-market and total exposure caps against house NAV,
  from protocol `V6` with a fixed dollar floor under each so trading opens from a zero or negative
  house
- Instant fills at the median of several venues, refused when quotes are stale or divergent
- Before protocol `V52`, zero spread at normal size and fixed-bps size spreads on position growth,
  widened under stress; from `V52`, every fill pays its market's size spread bands in tenths of a bp,
  on the slice that closes a position as on the slice that grows it, with no band below its class's
  floor (a tenth of a bp on FX and indices, 1bp elsewhere) and no stress multiplier below 1x
- Indicative quotes with bounded slippage: orders are declined only when the bound is exceeded
- A flat fee on realised profit only, and daily CFD financing at a benchmark rate plus markup
- USD accounts: markets quoted in euros, pounds, or yen accrue PnL in that currency and convert it to
  USD at the engine's own index for the FX market, and have no price while that FX market has none
- Tiered margin, account close-out largest loser first among markets that can trade, and negative
  balance protection; from protocol `V6`, close-outs also follow financing and parameter or listing
  changes, skip a market outside its session, and value a market that can trade at its live index
- Venue trading hours for RWA markets, with holidays, early closes, and special sessions, and
  resting orders filling at the gapped open
- Markout-based toxicity scoring that widens spreads for flow that picks off stale prices; from
  protocol `V54` a stick that only adds spread, measured over time so that round trips and size
  cannot dilute it
- Deposits credited at most once per chain reference, and all external numbers bounded
- A keccak-256 state root: from protocol `V9`, the root of a sparse Merkle tree over accounts,
  markets, and the rest of the state, which each root rehashes only where commands changed it;
  before, the hash of the canonical state encoding
- Proofs of an account's balance and positions, and of its markets' marks, against a root, laid out
  for a Solidity verifier (keccak-256, 32-byte nodes, explicit left and right) and checked without
  a node
- Versioned rules: a log activates each protocol version at a recorded seq, and a log that never
  does replays under `V2`, the rules every log starts under, to its original roots
- From protocol `V3`, client auth recorded in the log: EIP-712 signatures, per-account nonces, and
  expiries on every order, cancel, and withdrawal, checked by the engine so a replay proves each
  request was its account's, or unsigned requests in dev logs
- From protocol `V5`, position accounting rounds in the house's favour: an averaged entry is the
  exact average rounded once, up for a long and down for a short, realised and unrealised PnL round
  down, and the profit fee and financing charges round up, so no sequence of fills realises more
  than its exact PnL
- From protocol `V8`, admin commands signed by the operator key the log records, with the operator's
  nonces and expiries, limited to listings, parameters, benchmark rates, stress floors, and
  reduce-only; once any deposit or house capital is credited, an unsigned one is refused, so only
  genesis runs unsigned
- From protocol `V46`, a signed benchmark rate moves at most 300 bps per day from the applied rates
  next to its date (`src/benchmark.rs`), a market holding positions stays on its benchmark
  (`src/operator_bounds/`), and the operator's stress floor is at most level 1 and lapses after 72
  hours (`src/stress.rs`)
- From protocol `V49`, the stress parameters and a listed market's financing markup change only
  after a seven-day notice, as the profit fee and close-out level do (`src/operator_bounds/`)
- From protocol `V35`, the operator's powers are bounded (`src/operator_bounds/`). The profit fee
  and the close-out level change only within ±50% of their launch values, the rates in force when
  parameters are first set under `V35` or when it activates, and only from the time a signed
  `announce_params` names, at least seven days after it is logged (`NOTICE_MS`); a `set_params`
  that changes either rate otherwise is refused with `admin_rejected`. A relisting whose tighter
  price rules (more sources, shorter freshness, narrower divergence) give no index, as trading reads
  it, from the quotes the market keeps through the relisting is refused while accounts hold positions
  in the market, or in a market that converts at it, unless each such market is reduce-only. The
  operator key changes only by a signed `rotate_operator`, which takes effect seven days after it is
  logged: from then the old key signs nothing, and the new one signs under the same chain and vault.
  Earlier versions keep their rules
- From protocol `V38`, the state root commits what the vault's exit against the last posted root
  checks (`src/merkle/exit.rs`): a queued withdrawal's leaf and a receipt-backed pending deposit's
  leaf are ABI-encoded, and one leaf holds the vault deposit cursor, one past the highest deposit
  id in the root, with the pending and held principal the exit reserves, the principal of every id
  below the cursor the root does not hold, and the lowest 64 of those ids, which the vault refunds
  by id. Activation rebuilds
  the tree. `Engine::exit_proof` proves the terms, an account with its positions and their markets,
  its approvals, queued requests, distribution, and the pending deposits for it, against the last
  root, from committed copies, so a proof stays against its root after the state moves on, and
  `AccountProof::equity` values the account at the root's marks as `Engine::equity_and_margin` does
- From protocol `V37`, the operator key follows the operator registry's elections (`src/election.rs`).
  The registry's finalised `RotationChanged` and `Elected` events arrive as `switch_rotation` and
  `elect_operator` commands on the staking mirror, credited once under their chain references and
  advancing its authenticated frontier; an `election_rejected` event refuses one that contradicts
  the state the log holds. An election naming a key other than the one in effect records a pending
  rotation that takes effect at the election's effective time, or seven days after the election is
  logged when that is later, and only once a `staking_progress`, which carries its block time from
  `V37`, has reached the registry's effective time, so a cancellation always arrives first. Turning
  rotation on drops a signed rotation still pending, and while rotation is on `rotate_operator` is
  refused; turning rotation off with an election not yet in effect cancels the election and its
  rotation
- From protocol `V47`, while rotation is on, a fifth of each epoch's distribution credits the
  operator in effect when the close runs, in its candidate's cumulative distribution leaf, and stakers
  share the rest (`src/staking.rs`); from `V50` the fifth is split among the operators by their
  sequenced time in office during the epoch, and the time no one served goes to stakers
  (`src/staking/office.rs`)
- From protocol `V51`, daily operator turns (`src/turns.rs`): a registry that names a schedule adopts
  a rotation set at each election close and draws a turn for each 22:30 UTC boundary from it. The
  schedule's finalised `RotationSetAdopted` and `TurnAssigned` events arrive as `adopt_rotation_set`
  and `assign_turn` commands on the staking mirror, each credited once. A turn's admin key signs from
  its boundary, or from its mirror when that is later; a turn whose key no set mirrored seven days
  earlier names is credited and refused (`turn_refused`), keeping the key in effect. The member's node
  logs `turn_taken` with its first command in the turn, and while turns run the operator's share
  counts its time in office from then to the end of its turn. From `V53` a member's notice carries
  across the sets that name it unchanged, and the account in office before turns run holds it only
  until the first turn applies
- From protocol `V52`, a listed market's spread and margin bands move only as a listing notice of at
  least seven days names them (`src/listing_bounds.rs`): at every notional a spread rate stays from
  its launch rate to twice it and the margin requirement at or above launch's; a close-spread
  ceiling is installed or adopted only at or above its market's launch close charge, which counts the
  first band at the widest multiplier, and an adoption for a market not yet listed binds the listing
  its first listing must be
- From protocol `V55`, the profit fee falls with holding time (`src/holding_fee/`): a realised gain
  pays the larger of the base fee and a hold curve's fee at the hold since the position's open time,
  which an add moves to the size-weighted average with the open time counted as no older than the
  curve's last hold, from `V57` the launch curve's, whatever curve is in force; the base moves only up
  from launch, to +50%, and the curve only to one at or above its launch curve at every hold, each
  after a seven-day notice
- From protocol `V56`, parameter changes by stake-weighted operator vote (`src/governance/`): while
  rotation is on, the parameters, listings, toxicity weighting, close-spread adoptions, the access
  policy and protocol activation change only by a proposal the rotation set passes on the vote
  contract, mirrored as `pass_proposal` and credited once. A normal change waits seven days and
  applies when the node sequences `apply_proposal`; a fast-path proposal applies at once as a
  restriction that only raises a margin or the close-out level, sets a stress floor, lowers a cap, or
  makes a market reduce-only, and lapses at the recorded lifetime unless renewed
- From protocol `V59`, a market's weekend margin (`src/weekend/`): a rate each margin band charges
  at least over the market's weekend gap, any closure of at least 40 hours; from two hours before
  the gap starts until the market reopens, risk added in any market and withdrawals meet it, and when
  the gap starts an account below it is closed out on the markets whose gaps start then, largest
  loser first, at their marks; it moves only as a listing notice of seven days, or while rotation is
  on a passed vote, names it, and a calendar change that is not a vote may not narrow or remove a gap
  starting within seven days, nor add or bring forward a held market's gap within seven days

- One command that makes every market reduce-only, including markets listed later; trades that
  reduce or close a position fill in every reduce-only mode, and from `V8` parameters whose stress
  and toxicity spreads would reach the price are refused
- From protocol `V10`, spreads and margin round in the house's favour: every spread charge rounds up
  for the trader paying it, buying or selling, though a sell's stops a unit short of the price, and
  every margin requirement rounds up, from a position's notional to the close-out level
- Products and quotients that never overflow while the result fits, whatever the position size or
  price ratio
- From protocol `V11`, a toxicity score carries nine decimals of a bp and is the notional-weighted
  average of the account's markouts, from a neutral prior and over at most a memory of fill
  notional, so dust cannot reset a score and no single fill sets one; each markout is scored only at
  an index valid when it is scored, within one horizon of falling due
- From protocol `V12`, every command first brings each market to its sequenced time: an index that
  changes without a price update, at an open, when a quote ages out, or when its FX market gains a
  price, sets the mark and stress and fills resting orders and close-outs before any order,
  rollover, or check uses it, and a venue quote stamped ahead counts at once
- From protocol `V13`, stress follows its inputs: each open, priced market's automatic level is
  re-evaluated before every fill and after every command, a fast move counts within a trailing
  window, and a divergence trigger is a share of each market's divergence bound, so every market
  with a positive bound can reach every level; a market whose FX pair has no price keeps its
  level, since nothing fills there; activating `V13` turns absolute divergence triggers into
  shares of the widest listed bound
- From protocol `V14`, a market whose benchmark has no rate accrues no financing, and each rollover
  records the benchmarks that have none; once any benchmark has a rate, a listing that names one
  without is refused; each account rests at most `max_resting_orders` orders, a resting order that
  may add risk meets its market's minimum notional at its own price, and placing one checks only
  that order against the live price
- From protocol `V15`, a resting order that can only reduce the account's existing position in its
  market — its side opposing the position, its quantity at most the position's — skips the
  minimum-notional check, exactly as a reduce-only order does, so any position can be closed with a
  plain order; one that would flip the position, or that adds to it, still meets the minimum
- From protocol `V16`, parameters that set a smaller toxicity memory clamp every account's toxicity
  history to it, so a lowered memory already weighs the next markout against no more history than
  the memory, instead of that markout still weighing the history the account built
- From protocol `V17`, reopening a market keeps its trailing move window: prices observed within
  the window before a halt still count after the reopening, so a move across a halt shorter than
  the window raises fast-move stress as it would without the halt

- From protocol `V18`, dated decimal benchmark histories select each missed rollover's rate by its
  local date; observations older than five weekdays still apply and produce a stale status. Retained
  history covers unpaid rollovers and legal timezone/hour changes, including earlier local dates
- From protocol `V19`, MONKEY emissions: once the operator's signed initialisation records a start
  no earlier than itself and a period budget, each ten-minute period's budget is shared pro rata by
  the accounts' realised losses there (liquidations included, the profit fee and
  financing not), each fill's loss magnitude from `V43` — wins in the period do not offset — and the
  net realised loss before it,
  less debt the house wrote off (from `V32`, a write-off the period's losses cannot absorb is carried as
  debt, up to the weight the account earned, that later weight pays first), leaving out any account with negative cash at the
  cutoff; shares round down to base units and the rest, with every empty period's budget, is
  cancelled; budgets halve, rounded down, every 26,280 periods (182.5 days); the first command at or
  after a period's end finalises it before its own effects; and each account's cumulative
  entitlement is a state tree leaf with a proof the vault checks a claim against
- From protocol `V25`, MONKEY staking and USDC distributions: once the operator's initialisation
  records a liquidity floor, an epoch length, a start, and the unstake cooldown the staking
  contract enforces, each epoch the first command at or after its end closes it before its own
  effects, distributing the house NAV above the floor pro-rata to the staked snapshot in whole USDC
  base units that reduce house cash (epochs no command witnessed distribute nothing); stake and
  unstake mirror the staking contract's finalised events, credited once under their chain
  references; and each account's cumulative distribution is a state tree leaf with a proof the
  vault pays a claim against
- From protocol `V21`, no quote from outside a market's hours prices it: a quote is refused unless
  the market stays open from its stamp to its sequencing, quotes held from before a closure are
  dropped at the first command after it, and activation drops held quotes that could not count, so
  the first index after an open comes from quotes stamped after it
- From protocol `V21`, markets priced from front futures contracts: a listing names its contracts in
  roll order, the index is the front contract's median plus a roll adjustment, the gap between the
  front and next contracts is recorded whenever both have an index, and once the next contract's
  roll time has passed the market rolls onto it at the first command where it has an index, the
  adjustment taking the recorded gap, even after the front contract has stopped quoting
- From protocol `V22`, a withdrawal reserves whole USDC from trading cash in a public FIFO queue,
  and the queue approves its head, in part when physical cash is short, as an immutable payout with
  its own nonce. Payout capacity starts at the conserved cash whose principal is final and grows
  only with final deposit principal and house capital
- From protocol `V34`, a withdrawal request is at least 10 USDC (`MIN_WITHDRAWAL`, the vault's
  deposit floor) and is refused with `below_minimum_withdrawal` otherwise, so every request is
  bounded in count by what it reserves. The queue approves less than 10 USDC only as a request's final
  remainder, and otherwise waits for cash. Earlier versions keep dust requests and parts
- From protocol `V23`, signed corporate instructions capture prior-close stock/index entitlements,
  apply long-net/short-gross cash at the ex-date opening, and scale every split quantity/price
  atomically, rounding once and settling rounded-off fractions in cash, with chronological
  financing catch-up
- From protocol `V54`, the toxicity score only worsens a price (`src/toxicity/`): a fill's markout
  is the index's move from the fill's index 5 s later, in standard errors of its market's markout
  spread (the root mean square of the index's 5 s changes over the trailing hour, sampled on a grid
  of sequenced time and never below the floor the listing names); the score is the account's
  decayed weighted markout sum over the root of its decayed squared weights, with a 7-day half-life
  in whole hours and each weight capped at four times its decayed mean fill; from 20 decayed fills,
  tiers at scores of 4 and 8 add 2 and 5 bps; a fill matches the account's opposite fills within
  the horizon newest-first, what it closes against what they opened and what it opens against what
  they closed, and quantity matched at the same index scores nothing. The tiers
  move only after a seven-day notice, within ±50% of launch, and a floor is fixed at listing.
  Activation drops queued markouts and clears the `V11` scores, so every account starts neutral,
  and gives a market listed with no floor its asset class's

## Repository Structure

- `src/lib.rs` — module index and the protocol summary
- `src/engine/mod.rs` — engine state, command dispatch, sessions, and USD valuation
- `src/engine/house.rs` — house NAV, gross exposure, and the caps set from them
- `src/engine/margin.rs` — an account's equity and margin requirement at marks
- `src/engine/market.rs` — per-market state: quotes, marks, positions, resting orders, and futures
  roll position
- `src/hot_indexes/mod.rs` — derived holder, trigger, pending-receipt and above-frontier coverage indexes, rebuilt after recovery
- `src/hot_indexes/tests.rs` — the rebuild's coverage: the finalised base, whole skipped spans, and the runs a span merges
- `src/marks.rs` — venue prices, and marking a market and the markets that convert at its index
- `src/corporate/` — authenticated instruction types and scheduling (`mod.rs`), actual venue
  boundary validation (`calendar.rs`), checked dividend cash transfers and recorded-FX
  conversion (`cash.rs`), atomic split scaling with rounding and cash in lieu (`split.rs`), and
  chronological financing/action catch-up (`timeline.rs`)
- `src/execution/` — order placement, triggers, pricing, and booking fills (`mod.rs`), the margin
  and house exposure checks a fill that adds risk passes (`risk.rs`), the checks an order passes
  first (`validation.rs`), `Engine::quote`, which prices an order with
  those same functions and changes nothing (`quote.rs`; `tests/quote.rs` checks it against fills),
  and unit tests (`tests.rs`)
- `src/markouts.rs` — each fill's markout, queued and then marked against the index after its
  horizon, and toxicity scores before `V54`
- `src/toxicity/` — the `V54` toxicity score: its constants, tiers, sums, tier test and the reads
  of an account's standing (`mod.rs`), the hourly decay steps (`decay.rs`), each market's markout
  spread sampled on the 5 s grid (`spread.rs`), a fill's markout with its round trips and the score
  of due markouts (`marking.rs`), the index of the markouts a fill may match (`legs.rs`), the activation, floors and tier notices (`bounds.rs`), and unit
  tests (`tests.rs`)
- `src/merkle/toxicity.rs` — the `V54` score and markout spread leaves
- `src/closeout.rs` — account close-out and bad-debt write-off
- `src/benchmark.rs` — dated publisher snapshots, effective-date lookup, staleness, and retention;
  from `V46`, the cap on a signed rate (`split_by_cap`), which `check_held_benchmark` in
  `src/operator_bounds/` keeps a relisting from going around
- `src/funds.rs` — legacy and mock deposits, withdrawals, house capital, and overnight financing
- `src/deposits.rs` — exact custody receipts, sequencer-confirmed principal locks, finality,
  canonical reversals, and atomic moves of reincluded deposits to their replacement receipts
- `src/deposits/replacement.rs` — atomic canonical replacements and V26 payer screening with retained terminal holds
- `src/deposits/legacy.rs` — legacy credit provenance, activation principal locks, canonical adoption and exact finality lookup
- `src/deposits/legacy/tests.rs` — migration, independent release, ambiguous receipts and recovery commitments
- `src/deposits/tests.rs` — receipt transition, duplicate delivery, arithmetic and recovery tests
- `src/deposits/tests/coverage.rs` — canonical coverage, partial frontiers and restored index contracts
- `src/withdrawal.rs` — V22 FIFO reservations, bounded partial approval, the V34 minimum, and exact USDC conversion
- `src/merkle/withdrawal.rs` — payable approval leaves, queued request leaves, and their proofs
- `src/merkle/exit.rs` — the V38 exit terms and pending deposit leaves, the committed copies the exit
  proves from, account equity at the root's marks, `Engine::exit_proof`, and the V39 empty-index
  exclusion that names no unheld principal past the cursor
- `tests/exit.rs` — the exit leaves against `cast`, equity in every quote currency, proofs against
  the committing root, and the V38 rebuild; `tests/exit/custody.rs` the cursor and the pending and
  held totals over receipts, and the V39 sole reversed id-0 exclusion from `unheld`; `tests/exit/fixture.rs` the vault's vectors
- `tests/withdrawal_minimum.rs` — the V34 minimum request and the waiting of sub-minimum parts, against V33
- `tests/withdrawal_queue.rs` — reservations, partial FIFO, cash conservation, precision, and stale marks
- `tests/withdrawal_commitment.rs` — approval ABI, tamper rejection, serialization/replay, and incremental roots
- `tests/withdrawal_pagination.rs` — bounded scans, global positions, empty account continuations, and restart
- `tests/withdrawal_finality.rs` — payout capacity that waits for receipt finality and ignores reversals
- `tests/withdrawals/setup.rs` — shared Engine::apply-driven withdrawal setup
- `src/admin/mod.rs` — operator commands and signed admin commands; `src/admin/quote_currency.rs`
  checks a listing's quote currency
- `src/operator_bounds/mod.rs` — the V35 bounds: launch rates, rate notices and price-rule checks
- `src/operator_bounds/rotation.rs` — the V35 operator rotation: a signed `RotateOperator` and its seven-day notice
- `src/spread_tier.rs` — size spread bands: the `V52` unit in tenths of a bp beside the whole-bps
  encoding earlier listings keep, the class floors, and the band staircase; unit tests in
  `src/spread_tier/tests.rs`
- `src/listing_bounds.rs` — the V52 listing bounds: listing notices, launch bands, the curve checks a
  notice passes, and activation's conversion; unit tests in `src/listing_bounds/tests.rs`
- `tests/fill_spreads.rs` — `V52`'s size spread on every fill: round trips, partial closes through
  each band, reversals, the stress multiplier, reduce-only orders, close-all, closing limits, stops
  and close-outs, and a pure close at the widest admitted parameters
- `tests/listing_notice.rs` — `V52`'s listing notices and bounds, the class floors, the multiplier
  floor, activation's conversion, and a notice refused unsigned
- `tests/close_spread_floor.rs` — `V52`'s floor under the close-spread ceilings and the listing an
  adoption binds, the listing checks it is held to, and from `V58` its markout floor and the
  activation check across `V54`
- `src/close_spread.rs` — the V40 close-spread ceilings: the genesis registry, the seven-day
  adoption, the pricing-site clamp and reversal refusal, and the listing guards
- `src/holding_fee/mod.rs` — the V55 holding-time profit fee: the hold curve, its default, its shape
  and launch bounds, each position's open time, what a close pays now and when it next falls, and
  activation; unit tests in `src/holding_fee/tests.rs`
- `src/merkle/open_time.rs` — the V55 open-time leaves, kind 27, beside each position's leaf
- `tests/holding_fee.rs` — the fee at each hold, losses, adds, flips, close-outs, the margin check on
  a flip, close-all pieces, splits, activation, and what a close pays now
- `tests/holding_fee_bounds.rs` — the bounds on the base fee and the curve, their notices, the
  launch curve a fresh log's first parameters record, and the launch fee held from below at every hold
- `tests/holding_fee_clamp.rs` — `V57`'s add clamp at the launch curve's last hold: an add after a
  noticed curve extension, against `V55`'s clamp at the curve in force, activation over held open
  times, a launch curve longer than 15 minutes, and a fresh log launched with no curve
- `src/weekend/mod.rs` — the V59 weekend margin: each market's gap, when the margin applies, what it
  adds to a requirement, and the boundaries where a gap closes out and reopens; `src/weekend/schedule.rs`
  sets the gap at a listing, an activation and a reopening, and checks a listing's weekend margin and
  calendar
- `tests/weekend_margin.rs` — the lead-time refusal, the close-out at the gap's start at the last
  mark, the reopen, a market with none, a holiday that starts the gap early, and markets closing
  together
- `tests/weekend_margin_gap.rs` — what the margin holds over the gap: withdrawals and other markets'
  risk, a closure longer than a weekend or the search, a search that finds no gap run again at its
  end, a noticed weekend margin applied inside the gap, a market quoted in yen, and a catch-up
  deferred past a weekend close
- `tests/weekend_margin_bounds.rs` — the weekend margin's notice, its range, the calendar it needs,
  a calendar change that may not narrow or remove a gap within seven days, before or once it starts,
  held or not, nor add one to a held market within seven days, and before `V59` a weekend margin
  that does nothing until activation starts it, holding a gap it is in or that starts within the
  lead with no close-out, through a relisting and to a gap within the lead after it, with
  `tests/weekend_margin.rs` on the shared harness in `tests/common/weekend.rs`
- `tests/params_notice.rs` — the V49 notice on the stress parameters and on a listed market's markup: each refusal, each change from its time, a notice spent once, per-market notices, new listings, and replay
- `tests/operator_bounds.rs` — the V35 rate bounds, notices and rotation: each refusal and each change that takes effect after its notice
- `tests/operator_price_rules.rs` — the V35 price-rule bound: held markets, reduce-only, FX dependents, relisting and futures
- `src/election.rs` — the V37 elected operator key: the mirrored rotation switch and elections, and the pending rotation they record; from `V47`, the candidate seated and the account the operator's share goes to; from `V48`, the timelock-only installation the failed-key fallback mirrors
- `tests/operator_election.rs` — the V37 elections: refusals, the effective time and its seven-day floor, cancellation, and the refused signed rotation
- `src/governance/mod.rs` — the V56 parameter votes: the recorded vote contract, passed proposals mirrored once, the normal changes waiting out their notice and `ApplyProposal`, the governed commands, and the canonical command bytes a proposal carries
- `src/governance/restriction.rs` — the V56 fast-path restrictions: what one may carry, the values in force as the stricter of the baseline and every restriction, the baselines, renewal and lapse
- `tests/parameter_votes.rs` — `V56`: mirror refusals and crediting, a normal change no sooner than seven days after it is logged, the notices a vote replaces and the launch range it keeps, the operator's and the node's governed commands refused while rotation is on, fast-path refusals, the operator's levers beside a restriction, renewal and lapse, a normal vote under a restriction, and canonical bytes
- `tests/parameter_votes/baseline.rs` — `V56`: voted commands meeting their own checks, a voted change applying only over the baseline it passed on, parameters under a restriction moving only the baseline, renewal after the baseline moves, restricted stress levels, and the one waiting ceiling adoption
- `tests/parameter_votes/limits.rs` — `V56`: a refused drawn turn keeping votes governing until an installation, a restriction lapsing at a walked boundary, and the caps on waiting changes and restrictions
- `tests/parameter_votes/listing.rs` — `V56`: a voted relisting held to the listing bounds without a notice, the operator's listing notice refused while rotation is on, the size multiplier and close charge floors on voted changes, and a relisting under a margin restriction measured from the baseline
- `tests/parameter_votes/toxicity.rs` — `V56`: a voted score tier change held to ±50% of launch without a notice, and a voted relisting keeping its markout floor
- `tests/parameter_votes/holding_fee.rs` — `V56`: a voted curve held at or above its launch curve without a notice, and a voted base fee never below launch; from `V57` a voted extension lending an add no more than the launch curve's last hold
- `tests/parameter_votes/weekend.rs` — `V59`: a voted weekend margin with no listing notice, held to its listing's range, the operator's weekend margin notice refused while rotation is on, the fast path refusing to move it, and a fast-path margin raise past it
- `src/turns.rs` — the V51 daily turns: the mirrored rotation sets and turn assignments, the turn key's handover at its boundary or mirror, the seven days' notice a set gives its keys, carried across sets from V53, `TurnTaken`, and the turn's time in office
- `tests/daily_turns.rs` — `V51`: mirror refusals and crediting, kept sets, a drawn key from its boundary, a reassignment or promotion from its mirror, a key without notice refused, the refused signed rotation, `TurnTaken`, and the due key
- `tests/daily_turns/notice.rs` — `V53`: a member's notice carried across a late-mirrored set, refused under seven days and applied at exactly seven, and kept after its first set is pruned
- `tests/operator_share/daily_turns.rs` — `V51`: a member earns from its `TurnTaken` to its turn's end, and no one before or after; from `V53`, the account before the first turn only until that turn applies
- `tests/operator_install.rs` — `V48`: the timelock's installed operator: refusals, the seven-day log notice with no finalised frontier, replaced rotations, and the refused signed rotation
- `tests/operator_share.rs` — `V47`: the operator's fifth of a distribution, with rotation on and off, an epoch that distributes nothing, a built-up sweep, an operator that also stakes, and the leaf's claim and exit proofs
- `tests/operator_share/elections.rs` — who earns the share as elections, handovers, cancellations and signed rotations move the operator in effect
- `tests/operator_share/rounding.rs` — the share's rounding and its dust, as a table and a property
- `tests/operator_share/time_in_office.rs` — `V50`: the share split by time in office: a handover
  mid-epoch and in the close's own command, skipped epochs, an epoch no one served, rotation off for
  part of one, the count's start at activation, initialisation and each close, replay, and a property
  that the parts never pass the fifth nor the allocation the excess
- `src/emissions.rs` — MONKEY emissions: the schedule, period results, finalisation, and
  entitlements
- `src/staking.rs` — MONKEY staking and USDC distributions: the config, stake and unstake
  mirroring, epoch finalisation, the operator's share from `V47`, claimable distributions, and
  `distributable_excess`, the read-only figure an epoch close would distribute
- `src/staking/office.rs` — from `V50`, each account's time in office in the open epoch, and the
  split of the operator's share by it
- `src/staking/tests.rs` — staking tests: initialisation and validation, mirror crediting and
  refusals, epoch closes above and below the floor, rounding, skipped epochs, and the
  floor-preserving and principal locks
- `src/stress.rs` — stress levels: the automatic triggers, the move window, and the floors, from `V46`
  bounded at level 1 and lapsing after 72 hours
- `src/access/mod.rs` — signed eligibility, source policy and bounded public screening types
- `src/access/engine.rs` — deterministic screening, declarations and uncredited custody holds
- `src/auth.rs` — client auth, the operator record, request signatures and nonces, and the checks
  every client request and signed admin command pass, including from `V41` a registered session
  key within its scope
- `src/eip712.rs` — the EIP-712 domain, the request types clients and the operator sign, and signer
  recovery
- `src/protocol.rs` — protocol versions, and how to add one
- `src/merkle/mod.rs` — the state tree's layout and the sparse Merkle tree
- `src/merkle/leaf/mod.rs` — each leaf's key and value hash, and the account leaf;
  `src/merkle/leaf/market.rs` the market leaf
- `src/merkle/proof.rs` — account, market, and entitlement proofs, and how to check them
- `src/merkle/state.rs` — the engine's state tree: what each root rehashes, the remaining-state
  leaf, and distribution proofs
- `src/merkle/state/touched.rs` — tracked changes and complete leaf-key enumeration
- `src/merkle/proofs.rs` — account, entitlement, and withdrawal-approval proofs
- `src/merkle/tests.rs` — tree, proof, and work tests, and the layout fixture
- `src/tracked.rs` — maps and sets that record what changed since the last root
- `src/fixed/` — `Fixed`, the 9-decimal fixed-point type used for all money (`mod.rs`), and its
  unit tests (`tests.rs`)
- `src/market/mod.rs` — market listings, tier tables, the weekend margin's requirement, and quote
  currencies with their USD conversion; unit tests in `src/market/tests.rs`
- `src/params.rs` — protocol parameters, stress levels, toxicity, and the rollover clock
- `src/price.rs` — venue quotes and the reference index
- `src/futures.rs` — markets priced from futures: the contracts a listing names, which quotes the
  market takes, the roll-adjusted index, and the roll
- `src/session/mod.rs` — weekly trading windows, holidays, dated closures such as early closes, and
  special sessions; `src/session/weekend.rs` finds the next weekend gap, a closure of at least 40
  hours, and the one a time is in; unit tests in `src/session/tests.rs`
- `src/execution/batch.rs` and `src/execution/cancel.rs` — from `V44`, close-all and cancel-all: one
  signed request closes whole positions through the single reduce-only market order's path (from
  `V45` at most `Fixed::MAX_INPUT` units per entry), or cancels every resting order
- `src/session_keys.rs` — from `V41`, session-key registrations and revocations: their state,
  expiry cap, the order/cancel-only scope a session key signs within, and from `V42` the refilling
  budget of new addresses an account may introduce
- `src/account.rs` — accounts, netted positions, and resting orders
- `src/command/mod.rs` / `src/event/` (its tests in `tests.rs`, the typed refusals in `reject.rs`) — engine inputs and outputs
- `src/command/predicates.rs` — what kind of command a `Command` or `AdminCommand` is: who may sign it, and which activations and snapshots it carries
- `src/types.rs` — ids, sides, and asset classes
- `tests/` — protocol tests through `Engine::apply`, with a shared harness, its signing client
  (`client.rs`), protocol-version constructors (`versions.rs`), and the reference-log format in
  `tests/common/`; `tests/operator.rs` covers signed
  admin commands and every-market reduce-only, `tests/toxicity.rs` toxicity scoring, and
  `tests/quote_timing.rs` indices that change without a price update and venue quotes stamped ahead,
  `tests/stress.rs` stress levels from `V13`, `tests/stress_activation.rs` the activation of
  `V13` over earlier divergence triggers and move references, `tests/quote_hours.rs` quotes from
  outside a market's hours from `V21`, and `tests/futures.rs` markets priced from futures
- `tests/close_spread_cap.rs` — the V40 close-spread ceilings: the registry's adoption and
  governance path, the pricing-site clamp and reversal refusal, and the listing guards
- `tests/close_spread_calibration.rs` — fill, rounding and liquidation agreement vectors for the
  research-only close-spread model; shares input fixtures with the Python study
- `tests/rogue_operator.rs` — the two engine mechanisms `research/rogue-operator` rests on: free
  round trips at a zero spread tier holding a toxic account's score under the first tier, and a pair
  of opposite positions across an unpriced jump paying the move less the fee and the losing leg's
  balance, the rest bad debt; and from `V54` the same round trips leaving the score at its tier
- `tests/toxicity_score.rs` — `V54`: the add-on from 20 scored fills, an edgeless account at the
  best price, markouts from the fill's index, decay, close-outs left unmarked, a venue gap at the due
  time, and the markout spread's grid, window, floor, and closed and unpriced intervals, from `V58`
  an index quote expiry alone ends and brings back, with the shared harness in
  `tests/common/toxicity.rs`
- `tests/toxicity_round_trips.rs` — `V54` round trips: a fill closed in parts, adds closed by one,
  partial and flipping matches, a close and its reopening, another index or the horizon past,
  newest-first matching, close-outs, matching after recovery,
  and a toxic score through forty round trips
- `tests/toxicity_bounds.rs` — `V54` bounds: activation, its default floors and closing-spread check,
  floors at listing, tier notices within ±50%, the version's parameters, and the top add-on in the
  closing-spread and ceiling checks
- `tests/exit/toxicity.rs` — account leaves across the `V54` activation, with and without
  unfinalised principal, as the vault's exit hashes them
- `tests/access.rs`, `tests/access/` — signed declarations, source proofs, policy refusals, holds,
  resting orders covered by their placement screening, replay compatibility, and the `V24`
  reference session (`examples/reference_log/v24.rs`) with its assertions
- `tests/access/pending.rs` — durable retry of every curable refusal (source failures, a missing
  declaration, an unknown classification), exactly-once credit, and terminal sanctioned holds
- `tests/access/receipts.rs` — access screening of quorum-proven custody receipts: uncredited
  pending and held receipts stay out of cash and the withdrawal lock through finality, reversal,
  reinstatement and replacement
- `tests/fixtures/access-eip712.json` — independent wallet vectors for both declarations and source evidence
- `tests/reference/` — one reference log per protocol version: each line is a sequenced command and
  the state root after it
- `tests/corporate_actions.rs` and `tests/corporate_actions/` — signed scheduling, cash, split,
  calendar, missed rollover, restart and replay tests, with shared fixtures in `support.rs`
- `tests/reference_logs.rs` — replays every reference log and fails at the first seq whose root
  differs
- `tests/invariants/` — property tests over generated sessions (`main.rs`), the session generator
  (`session.rs`), the per-version checks each command must pass (`checks.rs`), fills at one price
  (`rounding.rs`), spreads and margin against their exact
  amounts (`charges.rs`), from `V52` the spread on every open, partial close and reversal against its
  exact amount (`closing_charges.rs`), and toxicity scores against their markouts (`toxicity.rs`)
- `tests/invariants/state_root.rs` — incremental roots against a rebuild, with every account's
  proof and entitlement proof checked, every emission budget accounted for, and no entitlement
  falling, under generated sessions
- `tests/invariants/access.rs` — explicit signing keys and fresh source evidence for current tree sessions
- `tests/state_root.rs` — the state root from `V9`: its activation, what each root rehashes,
  account proofs, and roots against a rebuild
- `tests/fixtures/eip712.json` — EIP-712 test vectors: API bodies, wallet typed data, and
  signatures
- `tests/deposit_finality.rs` — confirmed collateral, withdrawal headroom, reversal close-out, benchmark-safe activation and replay
- `tests/deposit_frontier.rs` — sequenced frontier refusals, financial preservation, settlement and replay
- `tests/fixtures/merkle.json` — state tree vectors computed with Foundry's `cast`: keys, leaf ABI
  encodings and value hashes, a root, and proofs
- `tests/fixtures/emissions.json` — entitlement proofs the engine produced, which the vault's
  Foundry tests claim with and `tests/emissions_fixture.rs` checks
- `tests/fixtures/exit.json` — exit proofs the engine produced, with the raw integers and the claims
  the vault's Foundry tests register and refund with, which `tests/exit/fixture.rs` checks
- `tests/access/emission_debt.rs` and `tests/corporate_actions/emissions.rs` — the `V32` emission debt
  (reversals in a later period, restart, root) and eligibility judged at the command's stamp during
  corporate-action catch-up.
- `tests/emissions.rs`, `tests/emissions_eligibility.rs`, `tests/emissions_schedule.rs` — MONKEY
  emissions from `V19`: allocation, rounding, cancellation, and finality; which losses count — gross
  loss magnitudes from `V43`, net before it — and the signed initialisation; and the schedule's
  arithmetic with the first halving
- `tests/staking_authentication.rs` — V27 mirror-identity roots, migration, and finalized scan anchors
- `tests/staking/withdrawal_reserve.rs` — shared distribution/FIFO capacity, inherited gaps, receipt lifecycle and exact reserves
- `tests/staking/composed_reserves.rs` — combined principal, marked liabilities, receipt reversal and settlement
- `tests/staking/marked_equity.rs` — closed-share FX insolvency, collectible NAV, settlement and activation boundaries
- `tests/staking/negative_cash.rs` — partial liquidation, unresolved negative cash, finality reserves, and protected claim liquidity
- `tests/staking.rs` — MONKEY staking and USDC distributions from `V25`: stake and unstake
  mirroring, epoch closes above and below the liquidity floor, pro-rata rounding, skipped epochs,
  and distribution proofs
- `tests/session_keys.rs` — `V41` session keys: registration and revival, per-key nonces, the
  order/cancel-only scope, revocations and expiries, the per-account cap and its pruning, and
  cross-account isolation, with the shared harness in `tests/common/session_keys.rs`
- `tests/session_keys_caps.rs` — session-key retention: the live cap and its pruning, `V41`'s
  lifetime cap, and `V42`'s refilling introduction budget (empty, hourly refill, carried partial
  hour, saturation, known addresses free, the activation crossing)
- `tests/batch_orders.rs` — `V44` batch orders: cancel-all and close-all events, the nonce, a replay,
  an empty, repeated or too long list, a flat or unlisted entry, expiry, screening, a session key,
  and the refusal before `V44`, with the shared harness in `tests/common/batch.rs`
- `tests/batch_max_input.rs` — `V45`: a close-all of a long or short position past the quantity
  input limit closes a piece of the limit per entry and leaves the rest in the fill's position; at
  the limit it closes whole; `V44` still refuses it
- `tests/batch_equivalence.rs` — property test: a close-all matches the same reduce-only orders sent
  one at a time, in events, balances, positions, market aggregates, and house cash
- `examples/reference_log/` — writes the reference log for a new protocol version: the session
  (`main.rs`), the requests and event names it uses (`helpers.rs`), its `V10` to `V12` parts
  (`v10.rs`, `v11.rs`, `v12.rs`), its `V13` part (`stress.rs`), its `V14` part (`v14.rs`), and its
  `V15` to `V57` parts (`v15.rs` to `v57.rs`)

- `examples/sign_request.rs` — a client signing a request body for the API
- `examples/verify_proof.rs` — checks an account proof from the API against a root

## Usage

```rust
use engine_core::Engine;
use engine_core::command::{Command, Sequenced};

let mut engine = Engine::new();
let events = engine.apply(&Sequenced { seq: 0, ts: 0, cmd: Command::Tick })?;
let root = engine.state_root();
```

The node crate (`engine-node`) stamps and logs every command before applying it; a verifier
replays that log with the same calls.

From `V9`, `Engine::account_proof` proves an account's leaf, and the leaf of each market it holds
a position in, against the last root computed, and `AccountProof::verify` checks it against a root
from a signed checkpoint. `src/merkle/mod.rs` and `src/merkle/leaf.rs` document the layout a
Solidity verifier follows. For a local dev node with a self-signed certificate, check a proof with:

```bash
curl --fail --silent --show-error --insecure https://127.0.0.1:8080/v1/accounts/<address>/proof |
    cargo run -p engine-core --example verify_proof -- <root>
```

The command above is for local development only. Against a remote node, verify its attestation
and pin its attested TLS key as described in the [TLS guide](../../README.md#running-it-locally).

A client signs each request with its account key under the EIP-712 domain of the vault, with the
account's next nonce and an expiry at most five minutes ahead:

```bash
SIGNER_KEY=<private key> cargo run -p engine-core --example sign_request -- \
    <chain id> <vault> <nonce> <expires at> \
    <order|cancel|cancel_all|close_all|withdrawal|eligibility|session_key_registration|session_key_revocation> \
    '<unsigned body>'
```

From V24, client actions and declarations always require signatures, including in development logs that recorded
unsigned auth. The declaration signs its exact professional or eligible-client statement and, like an
order or a withdrawal, carries the source's evidence for its account and peer. Source evidence is
also signed and checked during replay; [the source contract](../../config/access-source.md)
describes its domain, freshness and subject binding. Existing client request types keep their domain
version and original encodings.

The operator signs each admin command the same way, as its JSON under the vault's domain, with the
operator's next nonce; `engine admin`, in `engine-node`, signs and submits it.

## Configuration

`SIGNER_KEY`: the account's private key, hex, for the `sign_request` example.

`tests/benchmarks.rs` covers dated financing, catch-up, snapshots, staleness, and replay.
`tests/benchmark_retention.rs` covers calendar changes, retention boundaries, and dated financing.
`tests/benchmark_cap.rs` covers the `V46` cap: each limit, a date gap, a benchmark's first rate, a
refused snapshot, applied and backdated dates, undated rates, a market holding positions staying on
its benchmark, and what a refusal leaves financing.
`tests/benchmark_cap_history.rs` replays each launch benchmark's published history through the cap,
and requires what it refuses to match `research/benchmark-cap/results/refused.csv`.
`tests/stress_floor.rs` covers the `V46` stress floor: its limit, its 72 hours and their renewal, a
lapse at and before a walked rollover, activation over a higher floor, the automatic triggers and the
level a lapse leaves them at, reduce-only, and replay.

## Testing

```bash
cargo test -p engine-core
```

Two suites guard replay and accounting:

- `cargo test -p engine-core --test reference_logs` replays every log in `tests/reference/` and fails
  at the first seq whose state root differs from the committed one. CI runs it on x86_64 and
  aarch64 Linux, and `scripts/check-reference-logs.sh` fails a branch that changes, renames, or
  deletes a committed log.
- `cargo test -p engine-core --test invariants` runs 1,024 generated sessions from a fixed seed, each
  under `V2`, `V6`, `V7` with markets quoted in other currencies, `V11`, and `V12` and `V13` with
  those markets and signed operator commands, where `V13` and `V17` must reach a stress change, and `V14`,
  `V15`, `V16`, and `V17`.
  Each mixes client, feed, custody, and operator commands,
  with values inside and outside the input bounds, and checks after every command that nothing
  panicked, cash is conserved, and every market's aggregates match its accounts' positions. From
  `V12` it also checks that every market has processed the index it can deal at, holds no quote
  stamped after the sequenced time, and was filled only at that index and its FX market's. Raise
  `CASES` or change `SEED` in `tests/invariants/main.rs` to search further. The same suite generates
  adds, reductions, and flips at one price under `V5` and checks that none realises a profit; fills
  in random spread and margin bands under `V11` and checks that none pays less spread or posts less
  margin than the exact amount; and fills of any size from dust up, each marked out at a moved index
  or after a window without one, under `V11`, and checks that the toxicity score stays within a
  millionth of a bp of the exact notional-weighted average of its markouts. Under the latest
  version, the same sessions check that roots computed from what changed, at random points, equal a
  root hashed from scratch, and that every account's proof verifies against them. Under `V14` it
  also checks that resting orders keep to the cap, that placing an order fills or cancels no other,
  and that no market is financed at a rollover that records its benchmark as having no rate; from
  `V15` it also checks that a resting order accepted below its market's minimum notional, or with no
  dollar value, can only reduce its account's position in that market; under `V16`, that no
  account's toxicity history exceeds the memory in force.

Emission protocol V32 carries a write-off no period's realised losses can absorb forward as a
per-account emission debt, capped at the loss weight the account earned and was not yet charged
for, paid down by later loss weight before it earns again, and judges
emission eligibility during corporate-action catch-up at the enclosing command's stamp. Its
session is in `examples/reference_log/v32.rs`.

Protocol V38 commits what the vault's exit checks: ABI-encoded queue and pending deposit leaves, and
the exit terms leaf with the deposit cursor and the pending and held principal; its session is in
`examples/reference_log/v38.rs`.

Protocol V39 names no unheld principal past the exit terms' deposit cursor: with no live receipt
above the finalised frontier, a reversed deposit at vault deposit id 0 is the vault's cursor-rule
refund alone, not unheld too, so `startExit` no longer reserves it twice; its session is in
`examples/reference_log/v39.rs`.

Protocol V40 caps the spread a close can pay (`src/close_spread.rs`): a per-market ceiling registry
adopted at genesis, kept apart from `MarketSpec`, bounds every close whatever an operator command
does to the parameters, stress levels, toxicity tiers, or spread tiers. A pure close's charge
clamps to its ceiling, floored to the exact bound; a reversal whose blended spread would exceed
the ceiling is refused while the pure close stays open, and a triggered order refused that way
fills its closing slice and cancels the rest. After genesis a ceiling enters only by a signed
adoption in effect seven days after it is logged, and an entry can only be lowered; a listing
without an adopted ceiling, or one changing the instrument's identity, is refused; its session is
in `examples/reference_log/v40.rs`.

Protocol V41 adds session keys (`src/session_keys.rs`): one account-signed registration
authorises a browser-held key to place and cancel orders for the account until its expiry, capped
at 24 hours, or an account-signed revocation; the key's requests run on the key's own nonce
sequence under the same per-request expiry cap, a revoked or expired key and a key signing a
withdrawal refuse with their own rejections, and an account's keys are one pruned, capped leaf of
the state tree; its session is in `examples/reference_log/v41.rs`.

Protocol V42 replaces V41's lifetime cap on an account's distinct session-key addresses with a
budget that refills over sequenced time: a capacity of 100 new addresses, one returned per elapsed
hour with the part of an hour not yet earned carried, so a client that generates a key per
connect is never locked out for good and an account introduces at most 100 plus one per elapsed hour.
Re-registering a known address spends nothing. Activation starts each account's budget at 100 less
the addresses it introduced under V41, counting from the activation time; the leaf gains the
budget only from V42, so V41 roots are unchanged. Its session is in
`examples/reference_log/v42.rs`.

Protocol V43 makes MONKEY emission weight an account's gross realised losses in each period
(`src/emissions.rs`): every fill's loss magnitude counts, `max(-realised_pnl, 0)`, liquidations
included and the profit fee and financing still excluded, and wins in the period no longer offset
— before `V43` weight is the net realised loss there; the `V32` write-off debt still absorbs a
period's losses first, and what survives pays debt down before it earns; its session is in
`examples/reference_log/v43.rs`.

Protocol V44 adds batch orders (`src/execution/cancel.rs` and `src/execution/batch.rs`). `Command::CancelAll` cancels every
resting order the account holds, in order id order, each with the `OrderCancelled` a single cancel
emits; with none it does nothing. `Command::AccessCloseAll` (screened like an order) names
markets, each with the price the client saw, and a slippage bound: every entry runs, in list order,
as the reduce-only market order of the account's whole position in that market, through the same
validation and execution as one order sent alone, so its spreads, close-spread ceiling, profit fee,
emissions weight, stress and house caps are that order's, and each entry sees what the one before
left. An entry that cannot fill emits the rejection a single order would, with its list index as
the `client_id`, and the others still run; a market with no position is refused as reduce-only. A
list with no entries, naming a market twice, or with more entries than listed markets is refused whole. Both forms use
one nonce however many entries they hold, are authorised and screened once, and are signed as
`CancelAllRequest` and `CloseAllRequest` (the list as `CloseEntry[]`, in order). A session key signs
either form within its trade scope. Before V44 both are refused with `BatchUnsupported`, using no
nonce. Neither adds engine state, so V43 roots are unchanged. Its session is in
`examples/reference_log/v44.rs`.

Protocol V45 bounds a close-all entry's closing quantity at `Fixed::MAX_INPUT`. A position can
grow past that limit through repeated fills, and the closing order takes its quantity from the
position, so under V44 the order was refused as `qty out of range` and the position stayed open. From
V45 each entry closes at most `Fixed::MAX_INPUT` units, as repeated single reduce-only orders of
that size would, and the fill's `position` states what remains for the next request. A position
within the limit closes whole as before, and V44 logs replay unchanged. Its session is in
`examples/reference_log/v45.rs`.

Protocol V46 bounds what an elected operator can do with benchmark rates and the stress floor, since
its electors own the house NAV both move value towards. A signed rate more than 300 bps per day from
the applied rates next to its date is refused, in `set_benchmark_rates` and `SetBenchmarkRate` alike,
with the whole snapshot left unapplied. The limit is the cap times the calendar days between the
dates, so a real move refused today lands once enough days have passed, and a benchmark's first rate
is bound only by its ±100% range. A rate measured from an undated rate, which only a log from before
V18 can hold with no dated history, keeps one day's limit and is walked up by hand. Until a dated
rate is applied it binds only a first observation: a snapshot, or two in turn, that dates the undated
rate's own value far back and then the move today passes, since the move measures from the backdated
rate. V46 admits that, because a log written under it replays under the rules it was written under,
and it needs a log that set a rate before V18 and has set none since: a fresh node activates the
latest version before genesis, so its genesis rates are dated. A relisting cannot
move a market that holds positions onto another benchmark, since that would reach them with a first
rate; no limit on the move would do, as fresh names walk any limit within one second. A market with
no positions moves freely, and a retired benchmark's holders stay on its name, whose rates can be
signed within the cap. An applied date does not change, with a reason of its own: the rate
it holds passes, as the worker sends it again, and any other is refused, a publisher's revision
included, so no rate can be ratcheted, or walked from a backdated one, past the cap, except from the undated rate above. The last applied rate keeps financing meanwhile, and `benchmark_rate_stale`
reports it after five weekdays. `research/benchmark-cap/` checks the cap against each benchmark's
published history. The stress floor is at most level 1, in a market or in all of them, and lapses to
the automatic level 72 hours of sequenced time after it is set, unless it is set again, and a catch-up
prices the boundaries before that time with the floor. A floor above
level 1 when V46 activates drops to level 1, and each floor then in force starts its 72 hours. The
automatic triggers and reduce-only do not change, and earlier logs replay unchanged, since nothing
records a floor's time before V46. V46 does not bound a relisting's `financing.markup_bps`, or the
`stress_levels` and triggers in `SetParams`: both still change at once, until V49 gives them a
notice. Its session is in `examples/reference_log/v46.rs`.

Protocol V47 pays the operator a fifth of each epoch's distribution while rotation is on, through
the cumulative distribution leaf the vault's claim and exit already pay, so the vault does not
change. The share is the excess divided by five, rounded down, and stakers share the rest as before,
so what the share rounds off goes to them and their own rounding dust stays with the house; the
allocated total never passes the excess. It credits the operator in effect when the close runs, after
any handover due then: the candidate an election seated, while the key it was elected under is the
key in effect, or the address of the key for an operator no election installed, the identity the
registry gives its initial operator. A candidate is seated with its key, and also when an election
names the key already in effect. Earlier rules do not record the candidate, so an activation over a
last election not yet in effect, or cancelled, pays the key's address until the next election takes
effect; the version activates before rotation turns on, so a deployment never does. An operator that also
stakes holds both parts in one leaf. A close with rotation off, below the floor, or with nothing
staked credits the operator nothing, and a later close that sweeps an excess built up while nothing
was staked pays it a fifth of the sweep. Before V47 the candidate is not recorded, so earlier logs
replay unchanged. Its session is in `examples/reference_log/v47.rs`.

Protocol V49 gives the stress parameters and a listed market's financing markup the seven-day notice
the profit fee and close-out level already take, with no range. An `AnnounceParams` may name new
stress levels, with their spreads and triggers, a move window and a cooldown, which together decide
when a market reaches a level and how long it stays there; a `SetParams` that changes any of them
applies only those values, and only from the notice's time on, once. Like the rates, they share the
one pending parameter notice, which a new notice replaces whatever it names. `AnnounceMarkup` gives
one listed market's new markup, each market's notice kept and replaced apart, and a relisting that
changes it applies only that value, from its time on, once. A new listing needs no notice, since no
one holds it, and a relisting that keeps the markup leaves a pending notice waiting. Earlier versions
change both at once, and nothing records a stress or markup notice before V49, so earlier logs replay
unchanged. Its session is in `examples/reference_log/v49.rs`.

Protocol V50 splits the operator's share of each distribution among the operators by their sequenced
time in office, instead of paying all of it to the operator in effect at the close. While rotation is
on, each command first credits the time since the command before it to the account in office over
it, as V47 names that account and as the earlier command left it, so a handover counts from the
command that applies it. The open epoch counts from the command that closed the epoch before it, or
from V50's activation or staking's initialisation under it, to the command that closes it; skipped
epochs stay inside that count, as the close pays the excess they built up. The close divides the
fifth, `floor(excess / 5)`, by that count: each account earns `floor(fifth · served / counted)` in
its cumulative distribution leaf, with its own `OperatorShareAllocated`, in account order. The time
no one served, while rotation was off, and the parts' rounding go to stakers, so the allocated total
never passes the excess, and with rotation on throughout the parts sum to the fifth less under a
unit per operator. A handover in the close's own command leaves the whole epoch to the outgoing
operator, and the count restarts at every close, including one that distributes nothing. The epoch
the activation's own command closes still pays under V47's rule, and an activation from before V47
first seats the candidate of an election already in effect, which those rules never recorded. Before V50 nothing records time in
office, and the staking state encodes as before, so earlier logs replay unchanged. Its session is in
`examples/reference_log/v50.rs`.

Protocol V51 adds daily operator turns. A registry that names a schedule adopts a rotation set at
each election close, every eligible candidate weighted by its delegated stake, taking effect seven
days plus the finality margin later, each close's set after its own notice however many wait, and
anyone draws each day's turn from the set in effect a day before its 22:30 UTC boundary. The engine
mirrors the set and each turn assignment, with its position and its reason (the day's draw, a skip,
or a promotion), and keeps every set a turn yet to end can draw from. A turn's
key signs from its boundary when it is mirrored before it, and otherwise from its mirror, so a
reassignment after its boundary, and a promotion, which is mirrored only once finalised, apply from
then; the key in effect moves before the command is authorised. A turn whose candidate and key no set
mirrored at least seven days before the turn applies names, or whose turn is over when it is
mirrored, is credited but refused, and the key in effect stays. `RotateOperator` stays refused while
rotation is on, and an election is refused once a set is mirrored. The member's node logs
`TurnTaken { turn, release }` with its first command in the turn, and while turns run the operator's
share counts that member's time in office from it to the end of its turn: the time before a member
takes its turn, and an incumbent's time past its turn, go to stakers. An installation ends the turns.
Before V51 nothing records a set or a turn, and the state encodes as before, so earlier logs replay
unchanged. Its session is in `examples/reference_log/v51.rs`.

Protocol V52 makes every fill pay its market's size spread. The slice of a trade that closes a
position pays the market's bands over the notional it closes, as the slice that grows a position
always has, both at the stress level's multiplier, so a round trip pays the staircase twice, and
reduce-only orders, closing limits and stops, close-all and close-outs pay it too; a close-out still
pays no toxicity add-on. Bands are in tenths of a bp (`SpreadTier`, apart from the margin bands'
whole-bps `Tier`), and none is below its class's floor: a tenth of a bp on FX and index markets, 1bp
on every other. No stress level's size multiplier is below 1x. A pure close's charge clamps to its
ceiling before the check that a spread stays under the price, so a pure close always prices. A
listed market's spread bands, margin bands and markup move only as a listing notice names them, once
its seven days have run: `AnnounceListing` names any of the three, `AnnounceMarkup` the markup alone,
and each market keeps one notice. At every notional a noticed spread rate stays from the launch rate
to twice it, and the margin requirement, before each band's rounding, at or above launch's; the
launch bands are the listing in force at activation, or a market's first listing after it, and a new
listing needs no notice. A close-spread ceiling is at least its market's launch close charge, the
widest stress minimum and the top toxicity add-on plus the first band at the widest multiplier, in
tenths of a bp, under the parameters in force and under those the market launched with: an install
or adoption below it is refused, and an adoption for a market not yet listed names the listing it
binds, which that market's first listing must be, refused while its launch close charge exceeds the
ceiling. Activation writes every listed band in tenths of a bp, raises each to its floor and each
multiplier to 1x, and records the launch bands with each market's launch close charge; since a
ceiling only falls, it is refused while a listed market's ceiling or pending adoption is below that
charge, and such a log needs a fresh start. Bands listed earlier keep their whole-bps encoding,
a listing notice naming only a markup encodes as V49's did, and nothing else is recorded before V52,
so earlier logs replay unchanged. Its session is in `examples/reference_log/v52.rs`.

Protocol V53 keeps a member's notice across sets and counts no office past the first turn's application. A
set records, for each member the set mirrored before it named with the same key, when the log's
notice of that key began, so a member's notice runs from the first of the unbroken run of mirrored
sets naming its candidate with its key: a set mirrored late that names a member unchanged keeps its
notice, and so does a set the engine no longer keeps. A turn is checked against its own set for its
member and against that notice for the seven days. Before any turn is in effect, at the start of the
turns or after an installation ended them, the account in office holds it only until the first
accepted turn applies, so the first command past that boundary credits it nothing for the time since.
V53's activation carries the notice through the sets the log keeps then, as their adoptions under V53
would have. Before V53 no set records a carried notice, and the state encodes as before, so earlier
logs replay unchanged. Its session is in `examples/reference_log/v53.rs`.

Protocol V55 makes the profit fee fall with holding time (`src/holding_fee/`). A realised gain pays
the larger of `profit_fee_bps` and the parameters' `profit_fee_curve` at the hold of the slice it
closes, rounded up; a loss pays nothing. The curve is a table of `{hold_ms, fee_bps}` points, holds
ascending to at most 30 minutes and fees never rising: the first fee at and below the first hold,
linear between points with the fee rounded up to the bp, and zero at and past the last hold, where
the base alone applies. A hold runs from the position's open time (`Account::opened_at`, its own
state-tree leaf, kind 27, beside the position leaf the vault hashes): opening sets it to the fill's
time, an add moves it to the size-weighted average of now and the open time raised to no earlier
than now less the curve's last hold, rounded towards now, so the units held lend the add at most the
curve's length of age, a reduction leaves it, a flip closes the position at its hold and opens the
rest now, a close-out is a close, and a split keeps it. A risk-adding fill's margin check counts the fee at the closing slice's
hold, and a fill that closes some of a position names its hold and rate (`hold_ms`,
`profit_fee_bps`). From V55 `profit_fee_bps` moves only from its launch value up to +50%, and a curve
change, which `AnnounceParams` names, only to one at or above the launch curve at every hold before
rounding, each from its notice's time. A hold runs to the command's stamp, also for a close-out a
catch-up runs at an earlier boundary. Before V55 a curve in the parameters charges nothing and a
notice names none. Activation raises a base cut below its launch value back to it, gives parameters
that name no curve the default one, the full fee to 10 s falling to 10% at 15 minutes
(`DEFAULT_PROFIT_FEE_CURVE`), records the curve in force as the launch curve, and gives each held
position the open time `activation - the curve's last hold`, so its next close pays the base. No earlier log names a curve or keeps an open time, so earlier logs
replay unchanged. Its session is in `examples/reference_log/v55.rs`.

Protocol V56 adds parameter changes by stake-weighted operator vote. The node records the vote
contract (`ParameterVote`) and how long a fast-path restriction lasts, at most seven days, once. While
rotation is on, the parameters, listings, toxicity weighting, close-spread adoptions, the access
policy and protocol activation change only by a proposal the members of the rotation set in effect
when it opened pass with more than two thirds of the set's weight: the operator's signed commands for
them, and its parameter, markup and listing notices, are refused, and so are the node's own unsigned ones, so a
protocol activation waits for a vote. The engine mirrors each passed proposal, credited once, with the
admin command its bytes are the canonical JSON of. A normal change applies no sooner than seven days
after it is logged, when the node sequences `ApplyProposal`, under every check its command meets but
the notices the vote's own replaces, so the ±50% range of the launch profit fee and close-out level,
lower-only close-spread ceilings at or above the launch close charge, the size multipliers' 1x floor,
V52's listing bounds, V54's toxicity bounds and V55's holding-fee bounds still hold, a voted relisting keeping each changed band
table within its market's launch bands as a listing notice would and its markout floor, and voted
score tiers staying within ±50% of launch, a voted base fee at or above launch and a voted curve at
or above its launch curve; a `SetParams` or `UpsertMarket` applies only over the
baseline it was mirrored against, and a ceiling adoption only while none waits. A fast-path proposal applies at once as a
restriction, refused if any field moves away from lower risk: a higher margin band or close-out level,
a stress floor, a lower house or market cap or floor, or reduce-only. The values in force are the
stricter of the baseline and every restriction; only a normal vote, or the operator with rotation off,
moves the baseline, so a restriction's lapse leaves every later vote standing. The operator's own
stress floor and reduce-only sit beside the restrictions and cannot clear one. A restriction lapses
at its lifetime unless a vote on the same command, or the same bounds, renews it, and parameters that drop
a stress level a restriction holds a floor at are refused. Before V56 nothing records a vote, and the state
encodes as before, so earlier logs replay unchanged. Its session is in
`examples/reference_log/v56.rs`.

Protocol V57 clamps an add's averaged age at the launch curve's last hold, 15 minutes at launch: the
open time is raised to no earlier than now less the last hold of the curve `V55` recorded as the
launch curve (`OperatorBounds::launch_curve`), whatever curve is in force, so a curve that reaches
further, such as one extended to 30 minutes with a flat 10% tail, noticed or voted, cannot widen the
age an add borrows. Under V55 and V56 the clamp is the last hold of the curve in force, and so it
stays from V57 in a log launched with no curve, which has no hold to pin and whose bounds admit any
curve later. Activation keeps every held position's open time and records nothing, so earlier logs
replay unchanged. Its session is in `examples/reference_log/v57.rs`.

Protocol V58 checks the markout spread's index between commands and the markout floor of the
listings ceiling adoptions bind. The markout spread counts no change across a moment its market's
index lapsed by quote expiry alone: besides the grid points, price commands and relistings, the
sampler checks the index at each moment one of the market's quotes aged out since the previous
command, which where three or more venues quote can end the index and bring it back with no command
between. A close-spread ceiling adoption for a market not yet listed binds only a listing that names
a markout floor, since from V54 a first listing names one and must be the listing its adoption
bound. Activation gives each listing an adoption bound earlier, applied or pending, that names no
floor its asset class's default, as V54's activation gives a listed market's, and carrying the log
across V54 it is refused while the listing a market's first listing must be, the pending one where
an adoption waits, has a ceiling below its launch close charge under the score tiers the activation
converts to, as it is for a listed market. Before V58 none of this is checked or changed, so earlier
logs replay unchanged. Its session is in `examples/reference_log/v58.rs`.

Protocol V59 adds a market's weekend margin (`src/weekend/`, `MarketSpec::weekend_margin_bps`): each
of the market's margin bands charges at least that rate over its weekend gap, any closure of at
least 40 hours by its calendar (`Schedule::next_weekend_gap`, `Schedule::weekend_gap_at`), so a
holiday beside a weekend starts or ends the gap with it. The market keeps its next or current gap
(`MarketState::weekend`, in its leaf's state hash while set): its close, its reopening, and whether
its close-out has run. A listing sets it, the start of the gap closes out and the reopening moves it
on; both are boundaries the corporate timeline walks to, though not ones the `V33` catch-up bound
counts. From two hours before the gap starts until the market reopens, the account's margin
requirement counts each position in the market at its weekend margin for a fill that adds risk in
any market (`Reject::WeekendMargin`) and for a withdrawal; close-outs on price moves keep the normal
margin. At the gap's start, after any stress floor or restriction lapsing then and before captures,
the session refresh and any rollover at that time, each account holding a market whose gap starts
then and whose equity at its marks is below that requirement is closed out on those markets, largest
loser first, at their marks, until it meets it, as a close-out with its events. A relisting that
puts the market in a gap it has not closed out closes it out then. A listed market's weekend margin
moves only as a listing notice of seven days names it, or, while rotation is on, as a passed vote
does, whose own seven days replace the notice, to a rate from none to 10000 bps, on a calendar with
a gap within 366 days. A calendar change that is not a vote may not narrow or remove a weekend gap
the calendar in force gives within seven days, held or not, as new positions could otherwise carry
it at the normal margin; and while the market holds positions, it may not start a gap within seven
days that the calendar in force does not, which would close holders out at the weekend margin, a
margin raise with less notice than a rate change takes. A search that finds no gap within 366 days
keeps a frontier two hours before its end, applying no margin, and runs again there, in time for the
lead of a gap just past it. Before V59 a listing's weekend margin does nothing, as a field a later
version reads, no notice names one, and no market keeps a gap, so earlier logs replay unchanged;
activation starts each listed market that names one, holding a gap it is in, or one starting within
two hours, to its reopening with no close-out, through a relisting, and a gap starting within two
hours after one it entered the same way. Its session is in `examples/reference_log/v59.rs`.

Protocol V37 hands the operator key to the operator registry's elections while the registry has
rotation on: an election takes effect at its effective time, never less than seven days after it
is logged, and a signed rotation is refused; its session is in `examples/reference_log/v37.rs`.

Protocol V35 bounds the operator: rate changes within ±50% of the launch rates after a seven-day
notice, no price rules that leave a held market unpriced unless it is reduce-only, and an operator
key that rotates seven days after a signed rotation; its session is in
`examples/reference_log/v35.rs`.

Protocol V34 sets the minimum withdrawal request to 10 USDC and has the queue approve a part below
that only when it settles the request; its session is in `examples/reference_log/v34.rs`.

Staking protocol V31 reserves every allocation from physical withdrawal capacity, whether paid or
unpaid, and bounds new allocations by that unapproved capacity. Activation deducts cumulative
legacy allocations once; a negative inherited gap blocks new approvals until funded. Existing
approvals remain immutable and reserved. Receipt reversals do not unlock reserved distributions.

Staking protocol V30 composes reversible house and trader principal with marked liabilities.
For principal P, negative marked-equity deficits D, and negative cash C, the epoch ceiling is
min(NAV - P - D, house cash - P - C), less the liquidity floor and clamped at zero.
This retains principal and client gains together; cash debt is not charged twice. Crossing into
V30 installs the validated rules before closing a due epoch.

Staking protocol V29 discounts negative marked client equity from distributable NAV, including
positive-cash accounts whose losing positions cannot currently close. Its cap is the smaller of
collectible NAV and the funded-cash cap, so unresolved cash debt is not deducted twice. Epoch close
uses O(accounts + open positions + stakers) work. Crossing into V29 validates activation before
closing a due epoch, just as V28 does for its cash reserve rules.

Staking protocol V28 also reserves unresolved negative client cash: a partial liquidation cannot
distribute an unfunded loss while another position remains untradeable. Epoch close uses the existing
account reserve scan, followed by the staker allocation pass; it is O(accounts + stakers).
Crossing into V28 validates activation before closing a due staking epoch; a rejected activation
leaves that epoch pending, and a successful activation applies the funded-cash cap immediately.

Staking protocol V27 authenticates mirror identities as kind-15 leaves and completed finalized
scan anchors in the staking globals, while retaining V25 reference roots. V25 remains a legacy
pre-access rule set for replay; V24 and V26 onward enforce access screening.

### Adding a protocol version

A change to engine state, its serialization, or what a command does changes the roots that existing
logs replay to. It ships as a new protocol version, and the committed logs of earlier versions must
keep passing unchanged. `v1.jsonl` holds the original rules, and `v2.jsonl` adds two: a venue quote
stamped more than `MAX_QUOTE_LEAD_MS` after the sequenced time is refused, and any quote replaces a
stored one stamped after the sequenced time. Both predate activation, so every log starts under
their combined rules, which `src/protocol.rs` names `V2`; `v1.jsonl` still replays because none of
its quotes is stamped ahead. `V3` adds signed client requests and activates at a recorded seq; its
log is `v3.jsonl`. `V4` lets schedules carry dated closures and special sessions, and lets a zero
house capital move record its reference; it activates the same way, and its log is `v4.jsonl`. `V5`
rounds position accounting in the house's favour; its log is `v5.jsonl`. `V6` adds dollar floors
under the house exposure caps, close-out checks after financing and parameter or listing changes,
and a session check and live-index valuation on every close-out; its log is `v6.jsonl`. `V7` adds
markets quoted in another currency than USD (`MarketSpec::quote_currency`): their PnL, margin, house
exposure, and financing convert to USD at an FX market's index, rounding toward the house on top of
`V5`'s rounding, and they have no price while that FX market has none. Before `V7` activates, a
listing that declares another quote currency is refused, and a USD market has no FX mark and encodes
as before, so earlier logs replay unchanged; its log is `v7.jsonl`. `V8` adds admin commands signed
by the operator, refuses an unsigned operator command once anything is credited, adds every-market
reduce-only, and refuses parameters whose spreads would refuse a close; it activates the same way,
and its log is `v8.jsonl`. `V9` makes the state root the root of the state tree; its log is
`v9.jsonl`. `V10` rounds spreads and margin in the house's favour; its log is `v10.jsonl`. `V11`
makes a toxicity score the notional-weighted average of its markouts, to nine decimals, each scored
only at a valid index; its log is `v11.jsonl`. `V12` brings every market to its sequenced time before
each command and counts a quote stamped ahead at once; its log is `v12.jsonl`. `V13` makes stress
follow its inputs; its log is `v13.jsonl`. `V14` finances only markets whose benchmark has a rate
and caps each account's resting orders; its log is `v14.jsonl`. `V15` lets an order that can only
reduce the account's position in its market skip the minimum-notional check at placement; its log is
`v15.jsonl`. `V16` clamps every account's toxicity history when parameters set a lower memory; its
log is `v16.jsonl`. `V17` keeps a market's move window across a halt; its log is `v17.jsonl`.
`V18` adds dated benchmark snapshots and per-date catch-up. Its reference-log session is in
`examples/reference_log/v18.rs`. `V19` adds MONKEY emissions (`examples/reference_log/v19.rs`).
`V20` adds receipt-based custody principal locks, finality, reversal and reinclusion moves, with its
reference session in `examples/reference_log/v20.rs`. `V21` holds each quote to one open session
and prices markets from roll-adjusted futures contracts; its session is in
`examples/reference_log/v21.rs`. `V22` reserves withdrawals in a public FIFO queue paid from
physical cash; its session is in `examples/reference_log/v22.rs`.
`V24` adds signed eligibility declarations, screened orders and
withdrawals, and pending and held custody deposits and receipts. Its reference-log session is in
`examples/reference_log/v24.rs`. `V26` binds canonical receipt payers, screens replacements atomically,
and preserves terminal holds across transaction-local receipt variants. Activation refuses conflicting
legacy dispositions until a verified migration resolves them. Its session is in
`examples/reference_log/v26.rs`. `V33` keeps a quote that was stale under a market's freshness window
stale across a relisting that raises `max_age_ms`, checks held quotes for a closure only within that
window after the last command and at the end of a gap longer than it, and defers a command stamped more than
`MAX_CATCH_UP_BOUNDARIES` rollovers or corporate boundaries past the engine's schedule after it
catches up on that many (`Event::CatchUpDeferred`); the node sequences `Tick`s to finish the
catch-up before the command; its session is in
`examples/reference_log/v33.rs`.
For each later version:

1. Add the version in code as `src/protocol.rs` lists, then extend the session in
   `examples/reference_log/main.rs` so it activates the new version and exercises the new rules.
2. Write the version's log, which the generator refuses to do over an existing file:
   `cargo run -p engine-core --example reference_log -- crates/core/tests/reference/v<N>.jsonl`
3. Write the node's log from it, with the events each command emits:
   `cargo run -p engine-node --example reference_log -- crates/core/tests/reference/v<N>.jsonl
   crates/node/tests/reference/v<N>.jsonl`
4. Commit both files. The replay tests and CI pick them up with no other change.

The node's logs carry each command's events, and its replay refuses events that differ from what
the engine emits. A change to the events a command emits, or to how they serialize, is therefore a
protocol change too, even when state roots stay the same.

## Authors

The monkeytrading contributors

## License

Licensed under the Apache License, Version 2.0; see `LICENSE`.

## Sequencer command latency

Price updates use transient account-by-market and trigger-price indexes, and from `V54` a fill finds
the markouts it may match through a transient index by account, market, side and quantity left. Triggered orders retain
order-id priority, including limit orders whose spread prevents an immediate fill. Both indexes
are excluded from serialization and state roots, and rebuilt from accounts after recovery. They
apply to every historical protocol version without changing its events or roots.

Run `cargo test -p engine-core --test hot_path_benchmark --locked -- --ignored --nocapture`
in a quiet CPU window. The optimized dev profile benchmarks 100 samples per scenario; setup and
index recovery are outside steady-state timings. It measures `Engine::apply`, including event
allocation and drop, but excludes log I/O and checkpoint hashing, and signatures except in the
batch scenarios, whose requests are signed and so include the signature check, and which run under
the latest protocol version, so a close-all's fills pay every spread and fee rule in force. These figures are
command-processing regression guards, not an end-to-end service latency promise.

| Scenario | Accounts | p99 budget |
| --- | ---: | ---: |
| Dispersed book, one account in the repriced market | 1,000 and 100,000 | 200 us |
| Every account holds the repriced market | 100,000 | 250 ms |
| Dormant resting orders in the repriced market | 100,000 | 200 us |
| Daily rollover with financing on every position | 100,000 | 500 ms |
| Finalising a MONKEY emission period in which every account lost (one sample) | 100,000 | 500 ms |
| A signed empty cancel-all, the floor of a signed command (one sample per account) | 100,000 | 2 ms |
| A signed cancel-all of 100 resting orders | 100,000 | 2 ms |
| A signed close-all across the 37 launch markets | 100,000 | 2 ms |
| From `V54`, a signed fill among scored accounts with 2,000 markouts within the horizon | 100,000 | 2 ms |
| First command after a year-long gap with quotes held across it, or at the last stamp the engine accepts (one sample each) | 1 | 5 ms |

A concentrated price update still visits every holder, and a rollover still finances and checks
all positions. Triggered orders still require work per candidate, including orders retained when
the spread prevents their limit fill. The index adds memory proportional to positions and resting
orders; its one-time recovery cost is proportional to that authoritative state.

Measured on 23 October 2026 with rustc 1.96.0, the repository's optimized dev profile, incremental
compilation disabled, and two build jobs on a Mac17,9 with 15 logical CPUs and 24 GiB RAM:

| Scenario | Before p50 / p99 (us) | After p50 / p99 (us) |
| --- | ---: | ---: |
| Dispersed 1k | 3 / 6 | 1 / 5 |
| Dispersed 100k | 1,287 / 1,733 | <1 / 1 |
| Concentrated 100k | 22,631 / 23,597 | 20,554 / 24,040 |
| Dormant resting 100k | 12,656 / 16,346 | <1 / <1 |
| Financed rollover 100k | 33,341 / 37,129 | 32,669 / 34,991 |

Finalising a MONKEY emission period in which 100,000 accounts lost measured 35 ms, as one sample on
25 October 2026 on the same machine, with other workspace builds running.

The 23 October figures' baseline is main commit `e4b4f5cf79816f1167fb4c0e17dcd2ff0024cdb0`, with the same benchmark
source copied into its test directory. Measurements ran sequentially with competing workspace
builds paused. Budgets allow hosted-runner variation while rejecting the former dispersed-book
and dormant-order scans. They need verification on the PR's Linux runner before readiness.
When sharing a Cargo target across worktrees, ensure each crate actually recompiles from the
intended source: touching `crates/core/src/lib.rs` before each build avoids stale mtime-based
fingerprints. Verify the compile path in the build log.

The toxicity score's cost from `V54` (`toxicity_fill_latency` and `toxicity_trigger_latency` in
`tests/hot_path_benchmark/toxicity.rs`, each against `V52` running the same commands), as CI's `test`
job measured it on 13 November 2026, beside the other benchmarks: a signed fill among 100,000
accounts scored an hour before, with 2,000 markouts within the horizon and every launch market
holding an hour of spread history, 1,045 / 1,095 us p50 / p99 against 1,565 / 1,652 for
`V52`, which runs first in the same process; scoring 2,200 due markouts 2.0 ms against 0.8 ms, about 0.5 us a
markout, and the state root after it 50 ms against 47 ms; one price triggering 20,001 stops 227 ms
against 236 ms, no more a fill. Budgets: a fill's p99 2 ms, scoring 50 ms, the root after it 100 ms,
and the triggered fills twice `V52`'s plus 10 ms.
