//! Deterministic core of the house: a single-threaded state machine that turns sequenced commands
//! into events.
//!
//! The protocol:
//! - The house takes the other side of every trade. There is no order book and there are no
//!   external LPs. House exposure is capped per market and in total as a fraction of house NAV,
//!   from `V6` with a fixed dollar floor under each cap.
//! - Fills happen at once at a multi-venue index mid. Normal size pays no spread. Very large
//!   positions pay fixed-bps size spreads, which widen under stress.
//! - Losers pay nothing; winners pay a flat fee on realised profit. Positions held overnight pay
//!   or receive CFD financing.
//! - Margin is shared across the account at professional-client leverage, with close-out at 50% and
//!   negative balance protection. From `V6`, close-outs also follow financing and parameter or
//!   listing changes.
//! - RWA markets keep venue hours; nothing fills while a market is closed.
//! - Rules are versioned, and a log activates each version at a recorded seq. From `V3`, orders,
//!   cancels, and withdrawals are authorised as the log's client auth records: by the account's
//!   EIP-712 signature, next nonce, and expiry, or, in dev logs only, unsigned. From `V5`, every
//!   rounding in position accounting favours the house, so no sequence of fills realises more than
//!   its exact PnL. From `V8`, the operator signs admin commands as clients sign requests, with the
//!   key the log records, once anything is credited, and one command makes every market
//!   reduce-only. From `V11`, a toxicity score is the notional-weighted average of its account's
//!   markouts, to nine decimals of a bp, each scored only at an index valid when it is scored.
//! - Accounts hold USD. From `V7`, a market may be quoted in another currency: it accrues PnL in it,
//!   converts to USD at its FX market's index, and has no price while that FX market has none.
//! - From `V9`, the state root is a sparse Merkle tree over accounts, positions, markets, and the
//!   rest of the state, so a checkpoint rehashes only what changed, and an account's balance and
//!   positions can be proven against a posted root.
//! - From `V10`, every spread charge and margin requirement rounds up against the trader, though a
//!   sell's spread stops a unit short of the price.
//! - From `V12`, each command first processes every index that changed without a price update, so
//!   every fill and check uses the index the quotes give at its sequenced time, and a venue quote
//!   stamped ahead of that time counts at once.
//! - From `V13`, an open, priced market's stress level is re-evaluated before every fill and after
//!   every command, over a trailing move window, with divergence triggers scaled to each market's
//!   bound.
//! - From `V14`, no market accrues financing without a benchmark rate, and once any benchmark has
//!   one no market is listed onto a benchmark without; each account's resting orders are capped, a
//!   resting order that may add risk meets its market's minimum notional, and placing one checks
//!   only that order against the live price.
//! - From `V15`, a resting order that can only reduce the account's position in its market —
//!   opposite side, quantity at most the position — skips the minimum-notional check, as a
//!   reduce-only order does, so any position can be closed with a plain order.
//! - From `V16`, parameters that set a smaller toxicity memory clamp every account's toxicity
//!   history to it, so a lowered memory already weighs the next markout against no more history
//!   than the memory, instead of that markout still weighing the history the account built.
//! - From `V17`, reopening a market keeps its trailing move window: prices observed within the
//!   window before a halt still count after the reopening, so a move across a halt shorter than
//!   the window raises fast-move stress as it would without the halt.
//! - From `V19`, once the operator's signed initialisation records a start and a budget, MONKEY
//!   emissions share each ten-minute period's fixed budget pro rata by the accounts' net realised
//!   trading losses, less debt written off, halve the budget every 182.5 days, cancel what no
//!   account is allocated, and keep each account's cumulative entitlement in the state tree for the
//!   vault to mint against.
//! - From `V21`, a quote prices a market only from within one open session: it is refused unless
//!   the market stays open from its stamp to its sequencing, and dropped after any closure; a market
//!   may price from futures contracts, its index the front contract's median plus a roll adjustment
//!   that takes the gap between the contracts at each roll.

pub mod account;
mod admin;
pub mod auth;
pub mod benchmark;
mod closeout;
pub mod command;
pub mod deposits;
pub mod eip712;
pub mod emissions;
pub mod engine;
pub use engine::{Engine, SequenceError};
pub mod event;
mod execution;
pub mod fixed;
pub use fixed::Fixed;
mod funds;
pub mod futures;
mod hot_indexes;
pub mod market;
mod markouts;
mod marks;
pub mod merkle;
pub mod params;
pub mod price;
pub mod protocol;
pub mod session;
mod stress;
mod tracked;
pub mod types;
pub mod withdrawal;
