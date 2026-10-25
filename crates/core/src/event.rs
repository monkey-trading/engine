//! Engine outputs and the typed reasons a command is refused.

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::deposits::{DepositReceipt, DepositReject, DepositStatus};
use crate::emissions::TokenUnits;
use crate::price::PriceError;
use crate::types::{AccountId, MarketId, Millis, OrderId, Side};

/// Engine outputs. Clients and the custody layer act on events; they never
/// read state directly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Deposited {
        account: AccountId,
        amount: Fixed,
    },
    /// A deposit the engine refused to credit: out of range, or a reference already credited.
    DepositRejected {
        account: AccountId,
        amount: Fixed,
        chain_ref: String,
        reason: Reject,
    },
    CustodyDepositChanged {
        receipt: DepositReceipt,
        status: DepositStatus,
        /// External principal movement; adoption and finality do not move cash again.
        principal_delta: Fixed,
    },
    CustodyDepositRejected {
        receipt: DepositReceipt,
        reason: DepositReject,
    },
    WithdrawalApproved {
        account: AccountId,
        amount: Fixed,
        nonce: u64,
    },
    WithdrawalRejected {
        account: AccountId,
        amount: Fixed,
        reason: Reject,
    },
    HouseCapitalMoved {
        amount: Fixed,
        nav: Fixed,
    },
    OrderAccepted {
        account: AccountId,
        client_id: u64,
        order_id: OrderId,
    },
    OrderRejected {
        account: AccountId,
        client_id: u64,
        reason: Reject,
    },
    OrderCancelled {
        account: AccountId,
        order_id: OrderId,
        reason: CancelReason,
    },
    Fill(Fill),
    CloseOut {
        account: AccountId,
        equity: Fixed,
        margin: Fixed,
    },
    /// Loss beyond the account's balance, absorbed by the house
    /// (negative balance protection).
    BadDebt {
        account: AccountId,
        amount: Fixed,
    },
    Financing {
        account: AccountId,
        market: MarketId,
        amount: Fixed,
    },
    /// At a rollover from `V14`, a benchmark with no rate and the markets that use it, which accrue no
    /// financing for that day.
    BenchmarkRateMissing {
        benchmark: String,
        markets: Vec<MarketId>,
    },
    /// A legacy rate with unknown publisher date still applies and raises a Critical alert.
    BenchmarkRateUndated {
        benchmark: String,
    },
    /// The dated rate selected at a V18 rollover; stale observations still apply.
    BenchmarkRateObserved {
        benchmark: String,
        effective_date: chrono::NaiveDate,
        rollover_date: chrono::NaiveDate,
        stale: bool,
    },
    MarketOpened {
        market: MarketId,
    },
    MarketClosed {
        market: MarketId,
    },
    StressChanged {
        market: MarketId,
        level: usize,
    },
    /// A price update the engine ignored: an unknown market, a malformed venue quote, or a quote
    /// stamped more than `MAX_QUOTE_LEAD_MS` after the sequenced time.
    PricesRejected {
        market: MarketId,
        reason: String,
    },
    AdminApplied,
    AdminRejected {
        reason: String,
    },
    /// From `V19`, an account's share of a finalised emission period: `weight` is its eligible
    /// realised loss in USD, and `amount` the base units added to its entitlement.
    EmissionAllocated {
        account: AccountId,
        period: u64,
        weight: Fixed,
        amount: TokenUnits,
    },
    /// From `V19`, a finalised emission period: its budget, what accounts were allocated, and the
    /// remainder, cancelled for good.
    EmissionPeriodClosed {
        period: u64,
        budget: TokenUnits,
        allocated: TokenUnits,
        cancelled: TokenUnits,
    },
    /// From `V19`, the periods from `first` to `last` that no command fell in, whose budgets are
    /// cancelled for good.
    EmissionPeriodsCancelled {
        first: u64,
        last: u64,
        cancelled: TokenUnits,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reject {
    UnknownMarket,
    UnknownOrder,
    InvalidOrder {
        reason: String,
    },
    MarketClosed,
    ReduceOnly,
    /// The market has no index price. `error` is nested because `PriceError` carries its own `kind`
    /// tag.
    NoPrice {
        error: PriceError,
    },
    /// The market is quoted in another currency than USD, and its FX market has no price to convert
    /// at.
    NoFxPrice {
        fx_market: MarketId,
    },
    /// The live price is further from the client's quote than allowed.
    Slippage {
        quoted: Fixed,
        live: Fixed,
        slippage_bps: i64,
    },
    InsufficientMargin {
        equity: Fixed,
        required: Fixed,
    },
    InsufficientFunds {
        available: Fixed,
    },
    HouseCapacity,
    /// A limit or stop order from an account that already rests `max`, the parameters' cap.
    TooManyOrders {
        max: usize,
    },
    /// An amount at or below zero, or above `Fixed::MAX_INPUT`.
    InvalidAmount,
    /// A custody reference that was already credited.
    DuplicateReference,
    /// EVM credits under receipt finality require an exact custody receipt.
    CustodyReceiptRequired,
    /// An unsigned request, where the rules in force require a signature.
    MissingSignature,
    /// A signature that is not the account's over this request, or one the rules in force cannot
    /// check.
    InvalidSignature,
    /// A nonce other than the account's next: already used, or ahead of it.
    InvalidNonce {
        expected: u64,
    },
    /// A signed request sequenced after its expiry.
    Expired {
        expires_at: Millis,
    },
    /// A signed request whose expiry lies more than `MAX_EXPIRY_AHEAD_MS` past the sequenced time.
    ExpiryTooFar {
        latest: Millis,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    Client,
    /// Triggered but couldn't fill (margin, house capacity, or reduce-only
    /// with no position left).
    Unfillable,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub account: AccountId,
    pub order_id: OrderId,
    pub market: MarketId,
    pub side: Side,
    pub qty: Fixed,
    pub price: Fixed,
    /// Reference mid at fill time. `price` minus `index` is the spread paid.
    pub index: Fixed,
    pub spread_bps: i64,
    /// Price PnL realised by this fill, in USD, before the fee.
    pub realised_pnl: Fixed,
    /// The FX index `realised_pnl` converted to USD at; absent for a USD market.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx_index: Option<Fixed>,
    pub profit_fee: Fixed,
    /// Signed position after the fill.
    pub position: Fixed,
    pub liquidation: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `kind` tag of a JSON object. Deserializing fails when the object has two `kind` keys.
    #[derive(Deserialize)]
    struct Tag {
        kind: String,
    }

    #[test]
    fn no_price_rejection_has_one_kind_key_and_round_trips() {
        let cases = [
            (
                PriceError::Stale { fresh: 1, needed: 2 },
                r#"{"kind":"no_price","error":{"kind":"stale","fresh":1,"needed":2}}"#,
            ),
            (
                PriceError::Divergent { divergence_bps: 75 },
                r#"{"kind":"no_price","error":{"kind":"divergent","divergence_bps":75}}"#,
            ),
        ];
        for (error, expected) in cases {
            let reason = Reject::NoPrice { error };
            let json = serde_json::to_string(&reason).expect("rejections serialise");
            assert_eq!(json, expected);
            let tag: Tag = serde_json::from_str(&json).expect("exactly one kind key");
            assert_eq!(tag.kind, "no_price");

            // The event clients receive deserializes back to what the engine emitted.
            let event = Event::OrderRejected { account: AccountId([1; 20]), client_id: 7, reason };
            let json = serde_json::to_string(&event).expect("events serialise");
            assert_eq!(serde_json::from_str::<Event>(&json).expect("events deserialise"), event);
        }
    }
}
