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
- Zero spread at normal size, fixed-bps size spreads on position growth, widened under stress
- Indicative quotes with bounded slippage: orders are declined only when the bound is exceeded
- A flat fee on realised profit only, and daily CFD financing at a benchmark rate plus markup
- USD accounts: markets quoted in euros, pounds, or yen accrue PnL in that currency and convert it to
  USD at the engine's own index for the FX market, and have no price while that FX market has none
- Tiered margin, account close-out largest loser first among markets that can trade, and negative
  balance protection; from protocol `V6`, close-outs also follow financing and parameter or listing
  changes, skip a market outside its session, and value a market that can trade at its live index
- Venue trading hours for RWA markets, with holidays, early closes, and special sessions, and
  resting orders filling at the gapped open
- Markout-based toxicity scoring that widens spreads for flow that picks off stale prices
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
- From protocol `V35`, the operator's powers are bounded (`src/operator_bounds.rs`). The profit fee
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
  the accounts' net realised trading losses there (liquidations included, the profit fee and
  financing not), less debt the house wrote off (from `V32`, a write-off the period's losses cannot absorb is carried as
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

## Repository Structure

- `src/lib.rs` — module index and the protocol summary
- `src/engine/mod.rs` — engine state, command dispatch, sessions, and USD valuation
- `src/engine/house.rs` — house NAV, gross exposure, and the caps set from them
- `src/engine/market.rs` — per-market state: quotes, marks, positions, resting orders, and futures
  roll position
- `src/hot_indexes/mod.rs` — derived holder, trigger, pending-receipt and above-frontier coverage indexes, rebuilt after recovery
- `src/hot_indexes/tests.rs` — the rebuild's coverage: the finalised base, whole skipped spans, and the runs a span merges
- `src/marks.rs` — venue prices, and marking a market and the markets that convert at its index
- `src/corporate/` — authenticated instruction types and scheduling (`mod.rs`), actual venue
  boundary validation (`calendar.rs`), checked dividend cash transfers and recorded-FX
  conversion (`cash.rs`), atomic split scaling with rounding and cash in lieu (`split.rs`), and
  chronological financing/action catch-up (`timeline.rs`)
- `src/execution/` — order placement, triggers, pricing, risk checks, and booking fills (`mod.rs`),
  the checks an order passes first (`validation.rs`), `Engine::quote`, which prices an order with
  those same functions and changes nothing (`quote.rs`; `tests/quote.rs` checks it against fills),
  and unit tests (`tests.rs`)
- `src/markouts.rs` — each fill's markout, queued and then marked against the index after its
  horizon, and toxicity scores
- `src/closeout.rs` — account close-out and bad-debt write-off
- `src/benchmark.rs` — dated publisher snapshots, effective-date lookup, staleness, and retention
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
- `src/admin.rs` — operator commands and signed admin commands
- `src/operator_bounds.rs` — the V35 bounds: launch rates, rate notices, price-rule checks, and operator rotation
- `src/close_spread.rs` — the V40 close-spread ceilings: the genesis registry, the seven-day
  adoption, the pricing-site clamp and reversal refusal, and the listing guards
- `tests/operator_bounds.rs` — the V35 rate bounds, notices and rotation: each refusal and each change that takes effect after its notice
- `tests/operator_price_rules.rs` — the V35 price-rule bound: held markets, reduce-only, FX dependents, relisting and futures
- `src/election.rs` — the V37 elected operator key: the mirrored rotation switch and elections, and the pending rotation they record
- `tests/operator_election.rs` — the V37 elections: refusals, the effective time and its seven-day floor, cancellation, and the refused signed rotation
- `src/emissions.rs` — MONKEY emissions: the schedule, period results, finalisation, and
  entitlements
- `src/staking.rs` — MONKEY staking and USDC distributions: the config, stake and unstake
  mirroring, epoch finalisation, and claimable distributions
- `src/staking/tests.rs` — staking tests: initialisation and validation, mirror crediting and
  refusals, epoch closes above and below the floor, rounding, skipped epochs, and the
  floor-preserving and principal locks
- `src/stress.rs` — stress levels: the automatic triggers, the move window, and the floors
- `src/access/mod.rs` — signed eligibility, source policy and bounded public screening types
- `src/access/engine.rs` — deterministic screening, declarations and uncredited custody holds
- `src/auth.rs` — client auth, the operator record, request signatures and nonces, and the checks
  every client request and signed admin command pass, including from `V41` a registered session
  key within its scope
- `src/eip712.rs` — the EIP-712 domain, the request types clients and the operator sign, and signer
  recovery
- `src/protocol.rs` — protocol versions, and how to add one
- `src/merkle/mod.rs` — the state tree's layout and the sparse Merkle tree
- `src/merkle/leaf.rs` — each leaf's key and value hash, and the account and market leaves
- `src/merkle/proof.rs` — account, market, and entitlement proofs, and how to check them
- `src/merkle/state.rs` — the engine's state tree: what each root rehashes, the remaining-state
  leaf, and distribution proofs
- `src/merkle/state/touched.rs` — tracked changes and complete leaf-key enumeration
- `src/merkle/proofs.rs` — account, entitlement, and withdrawal-approval proofs
- `src/merkle/tests.rs` — tree, proof, and work tests, and the layout fixture
- `src/tracked.rs` — maps and sets that record what changed since the last root
- `src/fixed/` — `Fixed`, the 9-decimal fixed-point type used for all money (`mod.rs`), and its
  unit tests (`tests.rs`)
- `src/market.rs` — market listings, tier tables, and quote currencies with their USD conversion
- `src/params.rs` — protocol parameters, stress levels, toxicity, and the rollover clock
- `src/price.rs` — venue quotes and the reference index
- `src/futures.rs` — markets priced from futures: the contracts a listing names, which quotes the
  market takes, the roll-adjusted index, and the roll
- `src/session.rs` — weekly trading windows, holidays, dated closures such as early closes, and
  special sessions
- `src/session_keys.rs` — from `V41`, session-key registrations and revocations: their state,
  expiry cap, the order/cancel-only scope a session key signs within, and from `V42` the refilling
  budget of new addresses an account may introduce
- `src/account.rs` — accounts, netted positions, and resting orders
- `src/command.rs` / `src/event.rs` — engine inputs and outputs
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
  amounts (`charges.rs`), and toxicity scores against their markouts (`toxicity.rs`)
- `tests/invariants/state_root.rs` — incremental roots against a rebuild, with every account's
  proof and entitlement proof checked and every emission budget accounted for, under generated
  sessions
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
  emissions from `V19`: allocation, rounding, cancellation, and finality; which losses count and the
  signed initialisation; and the schedule's arithmetic with the first halving
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
- `examples/reference_log/` — writes the reference log for a new protocol version: the session
  (`main.rs`), the requests and event names it uses (`helpers.rs`), its `V10` to `V12` parts
  (`v10.rs`, `v11.rs`, `v12.rs`), its `V13` part (`stress.rs`), its `V14` part (`v14.rs`), and its
  `V15` to `V42` parts (`v15.rs` to `v42.rs`)

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
    <chain id> <vault> <nonce> <expires at> <order|cancel|withdrawal|eligibility> '<unsigned body>'
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

Price updates use transient account-by-market and trigger-price indexes. Triggered orders retain
order-id priority, including limit orders whose spread prevents an immediate fill. Both indexes
are excluded from serialization and state roots, and rebuilt from accounts after recovery. They
apply to every historical protocol version without changing its events or roots.

Run `cargo test -p engine-core --test hot_path_benchmark --locked -- --ignored --nocapture`
in a quiet CPU window. The optimized dev profile benchmarks 100 samples per scenario; setup and
index recovery are outside steady-state timings. It measures `Engine::apply`, including event
allocation and drop, but excludes log I/O, signatures and checkpoint hashing. These figures are
command-processing regression guards, not an end-to-end service latency promise.

| Scenario | Accounts | p99 budget |
| --- | ---: | ---: |
| Dispersed book, one account in the repriced market | 1,000 and 100,000 | 200 us |
| Every account holds the repriced market | 100,000 | 250 ms |
| Dormant resting orders in the repriced market | 100,000 | 200 us |
| Daily rollover with financing on every position | 100,000 | 500 ms |
| Finalising a MONKEY emission period in which every account lost (one sample) | 100,000 | 500 ms |
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

The baseline is main commit `e4b4f5cf79816f1167fb4c0e17dcd2ff0024cdb0`, with the same benchmark
source copied into its test directory. Measurements ran sequentially with competing workspace
builds paused. Budgets allow hosted-runner variation while rejecting the former dispersed-book
and dormant-order scans. They need verification on the PR's Linux runner before readiness.
When sharing a Cargo target across worktrees, ensure each crate actually recompiles from the
intended source: touching `crates/core/src/lib.rs` before each build avoids stale mtime-based
fingerprints. Verify the compile path in the build log.
