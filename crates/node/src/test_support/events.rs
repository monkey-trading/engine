//! One sample of every engine event, for tests that must cover each kind.

use alloy::primitives::B256;
use engine_core::Fixed;
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositReject, DepositStatus};
use engine_core::emissions::TokenUnits;
use engine_core::event::{CancelReason, Event, Fill, Reject};
use engine_core::price::PriceError;
use engine_core::types::{AccountId, Side};

/// One event of each kind, and an order rejection for each reason. The matches stop compiling when
/// a variant is added, until it has a sample here.
pub fn every_event() -> Vec<Event> {
    let (account, amount, market) = (AccountId([1; 20]), Fixed::from_int(5), 2);
    let reasons = [
        Reject::UnknownMarket,
        Reject::UnknownOrder,
        Reject::InvalidOrder { reason: "qty".into() },
        Reject::MarketClosed,
        Reject::ReduceOnly,
        Reject::NoPrice { error: PriceError::Stale { fresh: 1, needed: 2 } },
        Reject::NoPrice { error: PriceError::Divergent { divergence_bps: 30 } },
        Reject::NoPrice { error: PriceError::OutOfRange { mid: -amount } },
        Reject::NoFxPrice { fx_market: 10 },
        Reject::Slippage { quoted: amount, live: amount, slippage_bps: 5 },
        Reject::InsufficientMargin { equity: amount, required: amount },
        Reject::InsufficientFunds { available: amount },
        Reject::HouseCapacity,
        Reject::TooManyOrders { max: 100 },
        Reject::InvalidAmount,
        Reject::DuplicateReference,
        Reject::CustodyReceiptRequired,
        Reject::MissingSignature,
        Reject::InvalidSignature,
        Reject::InvalidNonce { expected: 3 },
        Reject::Expired { expires_at: 1_000 },
        Reject::ExpiryTooFar { latest: 2_000 },
    ];
    for reason in &reasons {
        match reason {
            Reject::UnknownMarket
            | Reject::UnknownOrder
            | Reject::InvalidOrder { .. }
            | Reject::MarketClosed
            | Reject::ReduceOnly
            | Reject::NoPrice { .. }
            | Reject::NoFxPrice { .. }
            | Reject::Slippage { .. }
            | Reject::InsufficientMargin { .. }
            | Reject::InsufficientFunds { .. }
            | Reject::HouseCapacity
            | Reject::TooManyOrders { .. }
            | Reject::InvalidAmount
            | Reject::DuplicateReference
            | Reject::CustodyReceiptRequired
            | Reject::MissingSignature
            | Reject::InvalidSignature
            | Reject::InvalidNonce { .. }
            | Reject::Expired { .. }
            | Reject::ExpiryTooFar { .. } => {}
        }
    }
    let fill = Fill {
        account,
        order_id: 3,
        market,
        side: Side::Sell,
        qty: amount,
        price: amount,
        index: amount,
        spread_bps: 4,
        realised_pnl: -amount,
        fx_index: Some(amount),
        profit_fee: Fixed::ZERO,
        position: amount,
        liquidation: true,
    };
    let mut events: Vec<_> =
        reasons.into_iter().map(|reason| Event::OrderRejected { account, client_id: 1, reason }).collect();
    let receipt = DepositReceipt {
        block: 1,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(2),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::Trader { account },
        amount,
    };
    events.push(Event::CustodyDepositChanged {
        receipt: receipt.clone(),
        status: DepositStatus::Unfinalised,
        principal_delta: amount,
    });
    events.push(Event::CustodyDepositRejected { receipt: receipt.clone(), reason: DepositReject::InvalidAmount });
    let house = DepositReceipt { beneficiary: DepositBeneficiary::House, ..receipt };
    events.push(Event::CustodyDepositChanged {
        receipt: house.clone(),
        status: DepositStatus::Unfinalised,
        principal_delta: amount,
    });
    events.push(Event::CustodyDepositRejected { receipt: house, reason: DepositReject::InvalidAmount });
    events.extend([
        Event::Deposited { account, amount },
        Event::DepositRejected { account, amount, chain_ref: "0xab:1".into(), reason: Reject::DuplicateReference },
        Event::WithdrawalApproved { account, amount, nonce: 1 },
        Event::WithdrawalQueued { account, amount, requested_seq: 12 },
        Event::WithdrawalQueueUpdated { account, requested_seq: 12, remaining: amount },
        Event::WithdrawalRejected { account, amount, reason: Reject::InsufficientFunds { available: amount } },
        Event::HouseCapitalMoved { amount, nav: amount },
        Event::OrderAccepted { account, client_id: 1, order_id: 3 },
        Event::OrderCancelled { account, order_id: 3, reason: CancelReason::Client },
        Event::OrderCancelled { account, order_id: 3, reason: CancelReason::Unfillable },
        Event::Fill(fill),
        Event::CloseOut { account, equity: amount, margin: amount },
        Event::BadDebt { account, amount },
        Event::Financing { account, market, amount: -amount },
        Event::BenchmarkRateMissing { benchmark: "SOFR".into(), markets: vec![market] },
        Event::BenchmarkRateUndated { benchmark: "SOFR".into() },
        Event::BenchmarkRateObserved {
            benchmark: "SOFR".into(),
            effective_date: chrono::NaiveDate::from_ymd_opt(2026, 10, 20).expect("valid fixture date"),
            rollover_date: chrono::NaiveDate::from_ymd_opt(2026, 10, 20).expect("valid fixture date"),
            stale: false,
        },
        Event::MarketOpened { market },
        Event::MarketClosed { market },
        Event::MarketRolled { market, contract: "ESH7".into(), adjustment: -amount },
        Event::StressChanged { market, level: 2 },
        Event::PricesRejected { market, reason: "stale quote".into() },
        Event::AdminApplied,
        Event::AdminRejected { reason: "bad params".into() },
        Event::EmissionAllocated { account, period: 3, weight: amount, amount: TokenUnits(250_000) },
        Event::EmissionPeriodClosed {
            period: 3,
            budget: TokenUnits(1_000_001),
            allocated: TokenUnits(1_000_000),
            cancelled: TokenUnits(1),
        },
        Event::EmissionPeriodsCancelled { first: 4, last: 9, cancelled: TokenUnits(6_000_006) },
    ]);
    for event in &events {
        match event {
            Event::Deposited { .. }
            | Event::DepositRejected { .. }
            | Event::CustodyDepositChanged { .. }
            | Event::CustodyDepositRejected { .. }
            | Event::WithdrawalApproved { .. }
            | Event::WithdrawalQueued { .. }
            | Event::WithdrawalQueueUpdated { .. }
            | Event::WithdrawalRejected { .. }
            | Event::HouseCapitalMoved { .. }
            | Event::OrderAccepted { .. }
            | Event::OrderRejected { .. }
            | Event::OrderCancelled { .. }
            | Event::Fill(_)
            | Event::CloseOut { .. }
            | Event::BadDebt { .. }
            | Event::Financing { .. }
            | Event::BenchmarkRateMissing { .. }
            | Event::BenchmarkRateObserved { .. }
            | Event::BenchmarkRateUndated { .. }
            | Event::MarketOpened { .. }
            | Event::MarketClosed { .. }
            | Event::MarketRolled { .. }
            | Event::StressChanged { .. }
            | Event::PricesRejected { .. }
            | Event::AdminApplied
            | Event::AdminRejected { .. }
            | Event::EmissionAllocated { .. }
            | Event::EmissionPeriodClosed { .. }
            | Event::EmissionPeriodsCancelled { .. } => {}
        }
    }
    events
}
