//! The key and value hash of each kind of leaf. Amounts and prices are `int128` in `Fixed`'s raw
//! units (9 decimals). Value hashes, in Solidity terms:
//! - **Account:** `keccak256(abi.encode(AccountLeaf))`, the struct below, where `openPositions`
//!   counts the account's position leaves; from `V11`, once a markout has scored the account, or its
//!   score carries a fraction of a bp, `keccak256(abi.encode(AccountLeaf, int128 toxicityFraction,
//!   int128 toxicityHistory))`, with the fraction above `toxicityBps` and the fill notional of history
//!   behind the score; from V20, while principal is unfinalised, the ABI suffix contains all three
//!   `int128` values: toxicity fraction, toxicity history, and unfinalised principal.
//! - **Position:** `keccak256(abi.encode(PositionLeaf))`, one per open position, keyed by its
//!   account and market.
//! - **Market:** `keccak256(abi.encode(MarketLeaf))`, with `marked` false and `mark` 0 before the
//!   market's first price; `quoteCurrency` 0 for USD, 1 for EUR, 2 for GBP, and 3 for JPY, with
//!   `fxMarket` the market whose index converts it to USD (0 for USD); `fxMarked` false and
//!   `fxMark` 0 until that index has priced it; and `stateHash` the keccak-256 of the JSON encoding
//!   of its spec, venue quotes, open and reduce-only flags, stress state, from `V12` the index it
//!   was last processed at, and from `V21`, for a market priced from futures, its roll state. A
//!   market's index of resting orders is left out: the order leaves hold those orders. An amount in
//!   a EUR or GBP market is `fxMark` times as many USD, and one in a JPY market `1 / fxMark` times
//!   as many.
//! - **Resting order:** the keccak-256 of the JSON encoding of `{ "account", "order" }`.
//! - **Request nonce:** `keccak256(abi.encode(uint64 nonce))`, the account's next nonce.
//! - **Custody receipt / transaction-local index:** the keccak-256 of the record's JSON encoding.
//! - **Legacy principal:** the keccak-256 of the `LegacyDeposit` JSON encoding, under the
//!   kind-12 key made from the historical custody reference's UTF-8 bytes.
//! - **Credited reference:** `keccak256(bytes(reference))`.
//! - **Markout:** the keccak-256 of its JSON encoding.
//! - **Emission entitlement:** `keccak256(abi.encode(uint256 entitled))`, the account's cumulative
//!   MONKEY entitlement in base units, from `V19`; the vault checks claims against this leaf.
//! - **Distribution:** `keccak256(abi.encode(uint256 entitled))`, the account's cumulative USDC
//!   distribution in base units, from `V25`; the vault checks claims against this leaf.
//! - **Staked balance:** `keccak256(abi.encode(uint256 staked))`, the account's staked MONKEY in
//!   base units, from `V25`, mirroring the staking contract's finalised events.
//! - **Emission period result:** `keccak256(abi.encode(int128 realised, int128 writtenOff))`, the
//!   account's realised PnL and written-off debt in the open emission period, from `V19`, until the
//!   period is finalised.
//! - **Emission ledger:** `keccak256(abi.encode(int128 earned, int128 debt))`, from `V32`: the loss
//!   weight the account earned that no write-off has yet been charged against, and the write-off
//!   debt later loss weight pays down.
//! - **Remaining state:** the keccak-256 of the JSON encoding of every engine field no other leaf
//!   holds (`state.rs`).
//!
//! ```solidity
//! struct AccountLeaf { int128 balance; uint64 withdrawalNonce; int64 toxicityBps; uint32 openPositions; }
//! struct PositionLeaf { int128 qty; int128 entry; }
//! struct MarketLeaf {
//!     bool marked; int128 mark; uint8 quoteCurrency; uint32 fxMarket; bool fxMarked; int128 fxMark;
//!     int128 clientQty; int128 clientCost; bytes32 stateHash;
//! }
//! ```
//!
//! No leaf grows with what a client accumulates: each position and resting order has a leaf of
//! its own. A market's quotes are keyed by the node's own feed sources, and its spec and the
//! remaining state are set by operator commands. JSON encodings are the ones the node's log uses
//! for the same types.

use std::collections::BTreeMap;

use alloy_primitives::{B256, U256};
use alloy_sol_types::SolValue;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::account::{Account, Position, RestingOrder};
use crate::emissions::{EmissionLedger, PeriodResult, TokenUnits};
use crate::engine::{MarketState, Markout, StressState};
use crate::futures::FuturesState;
use crate::market::{MarketSpec, QuoteCurrency};
use crate::merkle::keccak;
use crate::price::{Index, SourceQuote};
use crate::staking::UsdcUnits;
use crate::types::{AccountId, MarketId, Millis, OrderId};

/// The kind byte that starts each key.
const GLOBALS: u8 = 0;
const ACCOUNT: u8 = 1;
const MARKET: u8 = 2;
const REQUEST_NONCE: u8 = 3;
const CREDITED_REF: u8 = 4;
const MARKOUT: u8 = 5;
const RESTING_ORDER: u8 = 6;
const POSITION: u8 = 7;
const ENTITLEMENT: u8 = 8;
const PERIOD_RESULT: u8 = 9;
const RECEIPT: u8 = 10;
const RECEIPT_INDEX: u8 = 11;
const LEGACY_PRINCIPAL: u8 = 12;
const DISTRIBUTION: u8 = 13;
const STAKED: u8 = 14;
const STAKING_REFERENCE: u8 = 15;
const CORPORATE_ACTION: u8 = 21;
const CORPORATE_REFERENCE: u8 = 22;
const EMISSION_LEDGER: u8 = 23;
const ELIGIBILITY: u8 = 0x20;
const HELD_DEPOSIT: u8 = 0x21;
const PENDING_DEPOSIT: u8 = 0x22;

/// The structs whose ABI encodings account, position, and market leaves hash.
mod typed {
    use alloy_sol_types::sol;

    sol! {
        struct AccountLeaf {
            int128 balance;
            uint64 withdrawalNonce;
            int64 toxicityBps;
            uint32 openPositions;
        }

        struct PositionLeaf {
            int128 qty;
            int128 entry;
        }

        struct MarketLeaf {
            bool marked;
            int128 mark;
            uint8 quoteCurrency;
            uint32 fxMarket;
            bool fxMarked;
            int128 fxMark;
            int128 clientQty;
            int128 clientCost;
            bytes32 stateHash;
        }
    }
}

/// An account as its leaf holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountLeaf {
    pub balance: Fixed,
    /// Sequencer-confirmed principal excluded from withdrawal cash.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub unfinalised_deposits: Fixed,
    pub withdrawal_nonce: u64,
    pub toxicity_bps: i64,
    /// How many positions the account holds, each in a leaf of its own.
    pub open_positions: u32,
    /// From `V11`, the toxicity score's fraction of a bp above `toxicity_bps`. Left out of the
    /// encoding while zero.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub toxicity_fraction: Fixed,
    /// From `V11`, the fill notional of history behind the toxicity score. Left out of the encoding
    /// while zero; with the fraction, out of the hash while both are.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub toxicity_history: Fixed,
}

/// A market as its leaf holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketLeaf {
    /// The mark positions are valued at, once the market has been priced.
    pub mark: Option<Fixed>,
    /// The currency the market is quoted in, and the market whose index converts it to USD.
    pub quote_currency: QuoteCurrency,
    /// The index amounts convert to USD at, once that market has priced it.
    pub fx_mark: Option<Fixed>,
    pub client_qty: Fixed,
    pub client_cost: Fixed,
    /// keccak-256 of the JSON encoding of the market's spec, venue quotes, open and reduce-only
    /// flags, stress state, from `V12` the index it was last processed at, and from `V21` any roll
    /// state.
    pub state_hash: B256,
}

/// What a market's `state_hash` covers.
#[derive(Serialize)]
struct MarketRest<'a> {
    spec: &'a MarketSpec,
    quotes: &'a BTreeMap<String, SourceQuote>,
    open: bool,
    reduce_only: bool,
    stress: &'a StressState,
    /// Left out while `None`, as it is before `V12`, so earlier roots stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    processed_index: &'a Option<Index>,
    /// Left out while `None`, as it is for every market before `V21`, so earlier roots stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    futures: &'a Option<FuturesState>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    corporate_action_blocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_split_at: &'a Option<Millis>,
}

/// What a resting order's leaf covers.
#[derive(Serialize)]
struct OrderRest<'a> {
    account: AccountId,
    order: &'a RestingOrder,
}

impl AccountLeaf {
    /// The leaf of `account`.
    pub(crate) fn of(account: &Account) -> Self {
        // Every field is named, so a field added to `Account` fails to compile until a leaf holds it.
        let Account {
            balance,
            unfinalised_deposits,
            positions,
            orders: _,
            toxicity_bps,
            toxicity_fraction,
            toxicity_history,
            withdrawal_nonce,
        } = account;
        AccountLeaf {
            balance: *balance,
            unfinalised_deposits: *unfinalised_deposits,
            withdrawal_nonce: *withdrawal_nonce,
            toxicity_bps: *toxicity_bps,
            // Positions are keyed by `u32` market ids, so their count fits a `u32`.
            open_positions: u32::try_from(positions.len()).expect("one position per market id"),
            toxicity_fraction: *toxicity_fraction,
            toxicity_history: *toxicity_history,
        }
    }

    /// The toxicity score the leaf holds, in bps.
    pub fn toxicity_score(&self) -> Fixed {
        Fixed::from_int(self.toxicity_bps) + self.toxicity_fraction
    }

    /// The leaf's value hash.
    pub fn value_hash(&self) -> B256 {
        let leaf = typed::AccountLeaf {
            balance: self.balance.raw(),
            withdrawalNonce: self.withdrawal_nonce,
            toxicityBps: self.toxicity_bps,
            openPositions: self.open_positions,
        };
        if !self.unfinalised_deposits.is_zero() {
            let state = (self.toxicity_fraction.raw(), self.toxicity_history.raw(), self.unfinalised_deposits.raw());
            return keccak(&[&leaf.abi_encode(), &state.abi_encode()]);
        }
        if self.toxicity_fraction.is_zero() && self.toxicity_history.is_zero() {
            return keccak(&[&leaf.abi_encode()]);
        }
        let scored = (self.toxicity_fraction.raw(), self.toxicity_history.raw());
        keccak(&[&leaf.abi_encode(), &scored.abi_encode()])
    }
}

impl MarketLeaf {
    /// The leaf of `market`.
    pub(crate) fn of(market: &MarketState) -> Self {
        // `resting` indexes orders the order leaves hold.
        let MarketState {
            spec,
            quotes,
            open,
            reduce_only,
            mark,
            fx_mark,
            client_qty,
            client_cost,
            stress,
            resting: _,
            processed_index,
            futures,
            corporate_action_blocked,
            last_split_at,
        } = market;
        let rest = MarketRest {
            spec,
            quotes,
            open: *open,
            reduce_only: *reduce_only,
            stress,
            processed_index,
            futures,
            corporate_action_blocked: *corporate_action_blocked,
            last_split_at,
        };
        MarketLeaf {
            mark: *mark,
            quote_currency: spec.quote_currency,
            fx_mark: *fx_mark,
            client_qty: *client_qty,
            client_cost: *client_cost,
            state_hash: json_hash(&rest),
        }
    }

    /// The leaf's value hash.
    pub fn value_hash(&self) -> B256 {
        let (quote_currency, fx_market) = match self.quote_currency {
            QuoteCurrency::Usd => (0, 0),
            QuoteCurrency::Eur { fx_market } => (1, fx_market),
            QuoteCurrency::Gbp { fx_market } => (2, fx_market),
            QuoteCurrency::Jpy { fx_market } => (3, fx_market),
        };
        let leaf = typed::MarketLeaf {
            marked: self.mark.is_some(),
            mark: self.mark.unwrap_or(Fixed::ZERO).raw(),
            quoteCurrency: quote_currency,
            fxMarket: fx_market,
            fxMarked: self.fx_mark.is_some(),
            fxMark: self.fx_mark.unwrap_or(Fixed::ZERO).raw(),
            clientQty: self.client_qty.raw(),
            clientCost: self.client_cost.raw(),
            stateHash: self.state_hash,
        };
        keccak(&[&leaf.abi_encode()])
    }
}

/// The value hash of a position's leaf.
pub fn position_hash(position: &Position) -> B256 {
    // Every field is named, so a field added to `Position` fails to compile until the leaf holds it.
    let Position { qty, entry } = position;
    keccak(&[&typed::PositionLeaf { qty: qty.raw(), entry: entry.raw() }.abi_encode()])
}

/// The key of the leaf holding the engine's remaining state.
pub(crate) fn globals_key() -> B256 {
    keccak(&[&[GLOBALS]])
}

/// The key of `account`'s leaf.
pub fn account_key(account: AccountId) -> B256 {
    keccak(&[&[ACCOUNT], &account.0])
}

/// The key of `account`'s position in `market`.
pub fn position_key(account: AccountId, market: MarketId) -> B256 {
    keccak(&[&[POSITION], &account.0, &market.to_be_bytes()])
}

/// The key of `market`'s leaf.
pub fn market_key(market: MarketId) -> B256 {
    keccak(&[&[MARKET], &market.to_be_bytes()])
}

/// The key of a resting order's leaf.
pub(crate) fn order_key(order_id: OrderId) -> B256 {
    keccak(&[&[RESTING_ORDER], &order_id.to_be_bytes()])
}

/// The key of `account`'s request nonce leaf.
pub(crate) fn request_nonce_key(account: AccountId) -> B256 {
    keccak(&[&[REQUEST_NONCE], &account.0])
}

/// The key of a credited custody reference's leaf.
pub(crate) fn credited_ref_key(chain_ref: &str) -> B256 {
    keccak(&[&[CREDITED_REF], chain_ref.as_bytes()])
}

/// The key of an account's signed eligibility declaration.
pub(crate) fn eligibility_key(account: AccountId) -> B256 {
    keccak(&[&[ELIGIBILITY], &account.0])
}

/// The key of a permanently uncredited custody hold.
pub(crate) fn held_deposit_key(chain_ref: &str) -> B256 {
    keccak(&[&[HELD_DEPOSIT], chain_ref.as_bytes()])
}

/// The key of a deposit awaiting recovery of the screening service.
pub(crate) fn pending_deposit_key(chain_ref: &str) -> B256 {
    keccak(&[&[PENDING_DEPOSIT], chain_ref.as_bytes()])
}

/// The key of the markout at `index`.
pub(crate) fn markout_key(index: u64) -> B256 {
    keccak(&[&[MARKOUT], &index.to_be_bytes()])
}

/// The key of a pending corporate instruction and its entitlements.
pub(crate) fn corporate_action_key(id: &str) -> B256 {
    keccak(&[&[CORPORATE_ACTION], id.as_bytes()])
}

/// The key of a corporate identity retained after completion.
pub(crate) fn corporate_reference_key(id: &str) -> B256 {
    keccak(&[&[CORPORATE_REFERENCE], id.as_bytes()])
}

/// The value hash of `account`'s resting order.
pub(crate) fn order_hash(account: AccountId, order: &RestingOrder) -> B256 {
    json_hash(&OrderRest { account, order })
}

/// The value hash of a request nonce leaf.
pub(crate) fn request_nonce_hash(nonce: u64) -> B256 {
    keccak(&[&nonce.abi_encode()])
}

/// The value hash of a credited reference's leaf.
pub(crate) fn credited_ref_hash(chain_ref: &str) -> B256 {
    keccak(&[chain_ref.as_bytes()])
}

/// The value hash of a markout's leaf.
pub(crate) fn markout_hash(markout: &Markout) -> B256 {
    json_hash(markout)
}

/// The key of `account`'s emission entitlement leaf.
pub fn entitlement_key(account: AccountId) -> B256 {
    keccak(&[&[ENTITLEMENT], &account.0])
}

/// The value hash of an emission entitlement leaf.
pub fn entitlement_hash(entitled: TokenUnits) -> B256 {
    keccak(&[&U256::from(entitled.0).abi_encode()])
}

/// The key of `account`'s emission period result leaf.
pub(crate) fn period_result_key(account: AccountId) -> B256 {
    keccak(&[&[PERIOD_RESULT], &account.0])
}

/// The value hash of an emission period result leaf.
pub(crate) fn period_result_hash(result: &PeriodResult) -> B256 {
    // Every field is named, so a field added to `PeriodResult` fails to compile until the leaf holds it.
    let PeriodResult { realised, written_off } = result;
    keccak(&[&(realised.raw(), written_off.raw()).abi_encode()])
}

/// The key of `account`'s distribution leaf.
pub fn distribution_key(account: AccountId) -> B256 {
    keccak(&[&[DISTRIBUTION], &account.0])
}

/// The value hash of a distribution leaf.
pub fn distribution_hash(entitled: UsdcUnits) -> B256 {
    keccak(&[&U256::from(entitled.0).abi_encode()])
}

/// The key of `account`'s staked-balance leaf.
pub(crate) fn staked_key(account: AccountId) -> B256 {
    keccak(&[&[STAKED], &account.0])
}

/// The key of a credited staking mirror's identity, separate from custody deposits.
pub(crate) fn staking_reference_key(reference: &str) -> B256 {
    keccak(&[&[STAKING_REFERENCE], reference.as_bytes()])
}

/// The value hash of a staked-balance leaf.
pub(crate) fn staked_hash(staked: TokenUnits) -> B256 {
    keccak(&[&U256::from(staked.0).abi_encode()])
}

/// The key of `account`'s emission ledger leaf.
pub(crate) fn emission_ledger_key(account: AccountId) -> B256 {
    keccak(&[&[EMISSION_LEDGER], &account.0])
}

/// The value hash of an emission ledger leaf.
pub(crate) fn emission_ledger_hash(ledger: &EmissionLedger) -> B256 {
    keccak(&[&(ledger.earned.raw(), ledger.debt.raw()).abi_encode()])
}

/// keccak-256 of `value`'s JSON encoding.
pub(crate) fn json_hash(value: &impl Serialize) -> B256 {
    // Engine state is maps, lists, strings, and integers, all of which serde_json encodes.
    keccak(&[&serde_json::to_vec(value).expect("engine state serialises")])
}

/// The key of an exact custody receipt's lifecycle leaf.
pub(crate) fn deposit_key(reference: &str) -> B256 {
    keccak(&[&[RECEIPT], reference.as_bytes()])
}

/// The key of a transaction-local deposit's current receipt index.
pub(crate) fn deposit_transaction_key(reference: &str) -> B256 {
    keccak(&[&[RECEIPT_INDEX], reference.as_bytes()])
}

/// The key of legacy principal awaiting canonical receipt evidence.
pub(crate) fn legacy_deposit_key(reference: &str) -> B256 {
    keccak(&[&[LEGACY_PRINCIPAL], reference.as_bytes()])
}
