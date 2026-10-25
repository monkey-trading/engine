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
}

impl ProtocolVersion {
    /// The newest version this build implements.
    pub const LATEST: ProtocolVersion = ProtocolVersion::V20;

    /// Whether this rule set credits exact receipts with withdrawal principal locks.
    pub fn supports_deposit_finality(self) -> bool {
        self >= ProtocolVersion::V20
    }

    /// Whether this is `V2`, which engine state leaves out of its encoding.
    pub fn is_v2(&self) -> bool {
        *self == ProtocolVersion::V2
    }
}
