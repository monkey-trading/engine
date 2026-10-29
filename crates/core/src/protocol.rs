//! Protocol versions. A version is a set of rules. A log moves to a newer version with
//! `AdminCommand::ActivateProtocol`, so each command applies under the rules active at its seq, and
//! a log that never activates one replays under `V2`, the rules every log starts under. Versions
//! are named after their reference logs in `crates/core/tests/reference/`; `V2`'s rules took effect
//! from genesis before versions could activate, so `v1.jsonl` and `v2.jsonl` both replay under it.
//!
//! Adding a version:
//! 1. Add a variant after the last one and point [`ProtocolVersion::LATEST`] at it; the node
//!    activates `LATEST` on its next boot, at the next seq of each log. Activation is one-way: a
//!    build that predates the version cannot read a log that activated it, so a rollback needs a
//!    fresh log.
//! 2. Gate each changed rule on `self.protocol_version >= ProtocolVersion::Vn`, and leave the
//!    earlier path as it is.
//! 3. Keep the roots of earlier versions. Before `V9` the root hashes the engine's encoding, so
//!    skip new engine state from serialization while it holds its default (`skip_serializing_if`).
//!    From `V9` the root hashes the state tree's leaves (`merkle`): the leaf encodings name every
//!    field they cover and refuse to compile until new state has a leaf, so put it in one and
//!    encode it only while it differs from its default, or under the new version. A request type
//!    that changes gets new typed structs under a new `eip712::DOMAIN_VERSION`, chosen by protocol
//!    version, and the earlier ones stay so earlier signatures still verify. The operator signs
//!    an admin command as its JSON, so a field added to anything an admin command carries is
//!    skipped while it holds its default too, or logged signatures stop verifying.
//! 4. Write and commit the version's reference log, as `crates/core/README.md` ("Adding a protocol
//!    version") describes. Every earlier reference log must still replay to its roots.

use serde::{Deserialize, Serialize};

/// A rule set, in activation order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolVersion {
    /// The rules every log starts under: the original rules (`v1.jsonl`), with venue quotes stamped
    /// too far ahead of the sequenced time refused (`v2.jsonl`). Anyone may act for any account.
    #[default]
    V2,
    /// Orders, cancels, and withdrawals are authorised as the log's `ClientAuth` records: by the
    /// account's EIP-712 signature, next nonce, and expiry, or unsigned in dev logs.
    V3,
    /// Schedules may carry dated closures and special sessions, and a house capital move of zero
    /// records its reference without moving cash.
    V4,
    /// Position accounting rounds in the house's favour: an averaged entry is the exact average of
    /// the held entry and the fill price, rounded once, up for a long and down for a short; realised
    /// and unrealised PnL round down; and the profit fee and a financing charge round up.
    V5,
    /// Each house exposure cap is the larger of its share of house NAV and its fixed dollar floor.
    /// Close-outs are also checked after each rollover's financing and after a parameter or listing
    /// change, close out only markets inside their trading session, and value a market that can
    /// trade at the index they fill at.
    V6,
    /// A market may be quoted in EUR, GBP, or JPY, converted to USD at an FX market's index
    /// (`MarketSpec::quote_currency`), and an FX pair's own listing is quoted in its second currency,
    /// so a USD-JPY listing in USD is refused. Every other rule acts only through such a listing,
    /// which earlier versions refuse, so USD markets run as before.
    V7,
    /// Admin commands arrive signed by the `Operator` the log records, with the operator's next
    /// nonce and an expiry; once any deposit or house capital is credited, an unsigned operator
    /// command is refused. One command makes every market reduce-only, and no parameters may let
    /// stress and toxicity spreads price a closing trade at the price itself.
    V8,
    /// The state root is the root of the state tree (`merkle`), which each root updates where
    /// commands changed it, and which proves an account's leaf against a posted root.
    V9,
    /// Spreads and margin round in the house's favour: every spread charge rounds up for the trader
    /// paying it, from the size spread's notional and band charges to its share per unit, though a
    /// sell's stops a unit short of the price, so rounding never makes a spread reach the price; and
    /// every margin requirement rounds up, from the position's notional to the close-out level.
    V10,
    /// Toxicity scores carry nine decimals of a bp. A score is the notional-weighted average of its
    /// account's markouts, from a neutral prior and over at most a memory of fill notional
    /// (`ToxicityParams::notional`), and a markout is scored only at an index valid when it is
    /// scored, within one horizon of falling due. Markouts queue in due order.
    V11,
    /// Each command first brings every market to its sequenced time, after storing a price
    /// command's quotes: it refreshes every session, then processes each market whose index has
    /// changed since it was last processed, and the priced one, as a price update does, before the
    /// command or a rollover acts; a parameter or listing change does the same before its close-out
    /// check. A venue quote stamped ahead of the sequenced time counts at once, as stamped at the
    /// sequenced time.
    V12,
    /// Stress follows its inputs: an open, priced market's automatic level is re-evaluated before
    /// every fill and after every command, the fast-move trigger measures moves over a trailing
    /// window, and a divergence trigger is a share of each market's divergence bound. Activation
    /// turns absolute divergence triggers into shares of the widest listed bound.
    V13,
    /// A market whose benchmark has no rate accrues no financing, and each rollover records the
    /// benchmarks that have none; once any benchmark has a rate, no market is listed or moved onto
    /// one that has none. An account rests at most `max_resting_orders` orders, which parameters
    /// may set only from here, a resting order that may add risk meets its market's minimum notional
    /// at its own price, and placing one checks only that order against the live price.
    V14,
    /// A resting order that can only reduce the account's existing position in its market — its
    /// side opposing the position, its quantity at most the position's — need not meet the market's
    /// minimum notional, exactly as a reduce-only order need not, so any position can be closed
    /// with a plain order.
    V15,
    /// `SetParams` clamps every account's toxicity history to the memory in force, so a lowered
    /// memory weighs the next markout against no more history than that. Before this, an account's
    /// next markout after a lowered memory still weighs the history it had built under the old one;
    /// that markout caps the history at the memory then in force.
    V16,
    /// Reopening a market keeps its trailing move window: prices observed within the window before
    /// a halt still count after the reopening, so a move across a halt shorter than the window
    /// raises fast-move stress as it would without the halt. Before `V17` an open cleared the
    /// window.
    V17,
    /// Dated benchmark histories select each rollover's rate by its local date, including catch-up.
    V18,
    /// MONKEY emissions (`emissions`): once the operator's signed initialisation records a start and
    /// a budget, every ten-minute period's budget is shared pro rata by its eligible realised losses
    /// and finalised before the first command at or after its end, budgets halve every 26,280
    /// periods, and each account's cumulative entitlement is a leaf of the state tree.
    V19,
    /// Sequencer-confirmed custody receipts are trading collateral; Ethereum finality releases
    /// withdrawal principal, and canonical reversals remove the credited cash exactly once.
    V20,
    /// A quote is stored only if its market stays open from the quote's stamp to the sequenced time,
    /// quotes held from before a closure are dropped at the first command after it, and activation
    /// drops held quotes that could not count under these rules, so the first index after an open
    /// comes from quotes stamped after it. A listing may name futures contracts in roll order: its
    /// index is the front contract's median plus a roll adjustment; the next contract's quotes are
    /// taken too, with the gap between the two medians recorded whenever both have an index, and
    /// once the next contract's roll time has passed the market rolls at the first command where
    /// it has an index and a gap is recorded, the adjustment taking the gap.
    V21,
    /// A withdrawal reserves whole USDC from trading cash in a public FIFO queue, and the queue
    /// approves its head, partly when physical cash is short, as an immutable payout with its own
    /// nonce. Activation sets payout capacity to the conserved cash less principal awaiting
    /// finality, which joins it when its receipt finalises.
    V22,
    /// Pre-close corporate instructions capture entitlements and apply cash dividends and splits,
    /// rounding each scaled price and quantity once, before ex-date opening execution, in
    /// chronological order with financing. A failed split blocks its market and its financing.
    V23,
    /// Signed eligibility declarations and fail-closed API and custody access screening.
    V24,
    /// MONKEY staking and USDC distributions (`staking`): once the operator's initialisation
    /// records a liquidity floor, an epoch length, and a start, each epoch the house NAV above the
    /// floor is distributed to stakers pro-rata from the staked snapshot at the epoch's close,
    /// paid in whole USDC base units that reduce house cash, with each account's cumulative
    /// distribution a leaf of the state tree the vault pays a claim against. Stake and unstake
    /// mirror the staking contract's finalised events, each credited once under its chain
    /// reference.
    V25,
    /// Screen canonical replacement payers and retain terminal holds across receipt variants.
    V26,
    /// Staking mirror identities and completed finalized scan ranges are authenticated in the state root.
    V27,
    /// Reserve unresolved negative client cash before allocating staking distributions.
    V28,
    /// Exclude uncollectible marked client equity from distributable house NAV.
    V29,
    /// Compose reversible principal with marked liabilities before staking allocation.
    V30,
    /// Reserve staker distributions from unapproved physical withdrawal capacity.
    V31,
    /// An emission write-off that a period's realised losses cannot absorb, such as a reversal or
    /// bad debt landing after the losses it unwinds, becomes a per-account emission debt that later
    /// loss weight pays down before it earns again. Emission eligibility during corporate-action
    /// catch-up is judged at the enclosing command's stamp, not the rewound boundary.
    V32,
    /// A relisting that raises a market's `max_age_ms` first drops the quotes already stale under the
    /// old limit, so they cannot price again. A gap between two commands longer than the market's
    /// `max_age_ms` checks the held quotes for a closure only within that window after the last command
    /// and at its end, since after the window all of them are stale. A command stamped more than
    /// [`crate::corporate::MAX_CATCH_UP_BOUNDARIES`] rollover or corporate boundaries past the engine's
    /// last is deferred: it applies that many and nothing else, but for a benchmark batch's outcome.
    V33,
    /// A withdrawal request is at least 10 USDC, the vault's deposit floor, and the queue approves
    /// less than that only as a request's final remainder.
    V34,
    /// The profit fee and the close-out level move only within ±50% of their launch values and only
    /// from the time a signed notice, logged at least seven days before, names. A relisting whose
    /// tighter price rules leave a market with open positions without a price is refused unless the
    /// market is reduce-only. The operator key rotates only by a signed rotation that takes effect
    /// seven days after it is logged.
    V35,
}

impl ProtocolVersion {
    /// The newest version this build implements.
    pub const LATEST: ProtocolVersion = ProtocolVersion::V35;

    /// Whether access screening applies; the standalone legacy V25 rules predate access.
    pub fn supports_access(self) -> bool {
        self == Self::V24 || self >= Self::V26
    }

    /// Whether this rule set credits exact receipts with withdrawal principal locks.
    pub fn supports_deposit_finality(self) -> bool {
        self >= ProtocolVersion::V20
    }

    /// Whether this is `V2`, which engine state leaves out of its encoding.
    pub fn is_v2(&self) -> bool {
        *self == ProtocolVersion::V2
    }
}
