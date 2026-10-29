//! One sample of every engine event, for tests that must cover each kind.

use alloy::primitives::B256;
use engine_core::Fixed;
use engine_core::access::{AccessReject, EligibilityClass};
use engine_core::corporate::{Adjustment, CorporateAction};
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
        Reject::Access { reason: AccessReject::UnitedStates },
        Reject::PayoutUnavailable,
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
        Reject::BelowMinimumWithdrawal { minimum: amount },
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
        Reject::StakingNotInitialised,
        Reject::InsufficientStake,
    ];
    for reason in &reasons {
        match reason {
            Reject::Access { .. }
            | Reject::PayoutUnavailable
            | Reject::UnknownMarket
            | Reject::UnknownOrder
            | Reject::InvalidOrder { .. }
            | Reject::MarketClosed
            | Reject::ReduceOnly
            | Reject::NoPrice { .. }
            | Reject::NoFxPrice { .. }
            | Reject::Slippage { .. }
            | Reject::InsufficientMargin { .. }
            | Reject::InsufficientFunds { .. }
            | Reject::BelowMinimumWithdrawal { .. }
            | Reject::HouseCapacity
            | Reject::TooManyOrders { .. }
            | Reject::InvalidAmount
            | Reject::DuplicateReference
            | Reject::CustodyReceiptRequired
            | Reject::MissingSignature
            | Reject::InvalidSignature
            | Reject::InvalidNonce { .. }
            | Reject::Expired { .. }
            | Reject::ExpiryTooFar { .. }
            | Reject::StakingNotInitialised
            | Reject::InsufficientStake => {}
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
        uncredited: None,
    });
    events.push(Event::CustodyDepositRejected { receipt: receipt.clone(), reason: DepositReject::InvalidAmount });
    let house = DepositReceipt { beneficiary: DepositBeneficiary::House, ..receipt };
    events.push(Event::CustodyDepositChanged {
        receipt: house.clone(),
        status: DepositStatus::Unfinalised,
        principal_delta: amount,
        uncredited: None,
    });
    events.push(Event::CustodyDepositRejected { receipt: house, reason: DepositReject::InvalidAmount });
    events.extend([
        Event::CorporateActionScheduled {
            action: CorporateAction {
                id: "sample".into(),
                market,
                previous_close: 1000,
                ex_open: 2000,
                adjustment: Adjustment::Dividend { gross_per_unit: amount, withholding_rate: Fixed::from_bps(2500) },
            },
        },
        Event::CorporateEntitlementsCaptured { id: "sample".into(), market, at: 1000, accounts: 1 },
        Event::CorporateDividend { id: "sample".into(), market, account, quantity: amount, amount, fx_mark: None },
        Event::CorporateSplit { id: "sample".into(), market, new_units: 4, old_units: 1 },
        Event::CorporateCashInLieu { id: "sample".into(), market, account, amount },
        Event::CorporateActionApplied { id: "sample".into(), market, at: 2000 },
        Event::CorporateActionFailed { id: "sample".into(), market, at: 2000, reason: "inexact split".into() },
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
        Event::OrderCancelled { account, order_id: 3, reason: CancelReason::Split },
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
        Event::EligibilityAttested { account, class: EligibilityClass::Professional },
        Event::EligibilityRejected { account, reason: Reject::InvalidSignature },
        Event::DepositPending {
            account: Some(account),
            payer: Some(account),
            amount,
            chain_ref: "pending".into(),
            reason: AccessReject::SourceUnavailable,
        },
        Event::DepositHeld {
            account: Some(account),
            payer: Some(account),
            amount,
            chain_ref: "held".into(),
            reason: AccessReject::PayerSanctioned,
        },
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
        Event::Staked { account, amount: TokenUnits(7) },
        Event::Unstaked { account, amount: TokenUnits(7) },
        Event::StakingRejected {
            account,
            amount: TokenUnits(7),
            chain_ref: "0xcd:2".into(),
            reason: Reject::InsufficientStake,
        },
        Event::DistributionAllocated { account, epoch: 3, amount: engine_core::UsdcUnits(4_000) },
        Event::DistributionEpochClosed {
            epoch: 3,
            nav: amount,
            floor: amount,
            excess: engine_core::UsdcUnits(4_001),
            allocated: engine_core::UsdcUnits(4_000),
        },
        Event::DistributionEpochsSkipped { first: 4, last: 9 },
        Event::RotationSwitched { enabled: true, election_cancelled: false },
        Event::OperatorElected { epoch: 3, candidate: account, admin_key: account, effective_at: 1_793_000_000_000 },
        Event::ElectionRejected { chain_ref: "0xef:4".into(), reason: "rotation is off".into() },
        Event::CatchUpDeferred { due: 1_793_000_000_000 },
    ]);
    for event in &events {
        match event {
            Event::CorporateActionScheduled { .. }
            | Event::CorporateEntitlementsCaptured { .. }
            | Event::CorporateDividend { .. }
            | Event::CorporateSplit { .. }
            | Event::CorporateCashInLieu { .. }
            | Event::CorporateActionApplied { .. }
            | Event::CorporateActionFailed { .. }
            | Event::EligibilityAttested { .. }
            | Event::EligibilityRejected { .. }
            | Event::DepositHeld { .. }
            | Event::DepositPending { .. }
            | Event::Deposited { .. }
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
            | Event::EmissionPeriodsCancelled { .. }
            | Event::Staked { .. }
            | Event::Unstaked { .. }
            | Event::StakingRejected { .. }
            | Event::DistributionAllocated { .. }
            | Event::DistributionEpochClosed { .. }
            | Event::DistributionEpochsSkipped { .. }
            | Event::RotationSwitched { .. }
            | Event::OperatorElected { .. }
            | Event::ElectionRejected { .. }
            | Event::CatchUpDeferred { .. } => {}
        }
    }
    events
}
