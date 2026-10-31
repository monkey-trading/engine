//! Node metrics in Prometheus format. The registry belongs to the node: `main` builds one and hands
//! it to the sequencer, the monitor, and the alert router, and the operator listener serves it.
//! Counters and gauges are atomics. The histogram, and the first use of a label set, take a lock
//! that encoding also takes, so such a write can wait for a scrape to finish encoding that metric.
//! Labels name markets, venues, and reasons, never accounts.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use axum::Router;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::get;
use engine_core::Fixed;
use engine_core::deposits::DepositReject;
use engine_core::event::{Event, Reject};
use engine_core::types::MarketId;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

const CONTENT_TYPE_OPENMETRICS: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";
/// Every `kind` a typed rejection reason can have; see `reject_kind`.
const REJECT_KINDS: [&str; 25] = [
    "access",
    "payout_unavailable",
    "unknown_market",
    "unknown_order",
    "invalid_order",
    "market_closed",
    "reduce_only",
    "no_price",
    "no_fx_price",
    "slippage",
    "insufficient_margin",
    "insufficient_funds",
    "below_minimum_withdrawal",
    "house_capacity",
    "too_many_orders",
    "invalid_amount",
    "duplicate_reference",
    "custody_receipt_required",
    "missing_signature",
    "invalid_signature",
    "invalid_nonce",
    "expired",
    "expiry_too_far",
    "staking_not_initialised",
    "insufficient_stake",
];
/// Every reason a price rejection can have; see `prices_reject_reason`.
const PRICES_REJECT_REASONS: [&str; 6] =
    ["unknown_market", "malformed_quote", "quote_ahead", "market_closed", "contract_not_taken", "other"];
/// Every custody receipt rejection reason; see `deposit_reject_kind`.
const DEPOSIT_REJECT_KINDS: [&str; 7] = [
    "protocol_not_active",
    "invalid_receipt",
    "invalid_amount",
    "arithmetic_overflow",
    "unknown_receipt",
    "conflicting_receipt",
    "settled_receipt",
];
/// Every delivery outcome of an alert page.
const DELIVERY_OUTCOMES: [&str; 3] = ["sent", "failed", "dropped"];

/// A gauge holding a float: amounts, prices, and timestamps in seconds.
pub type FloatGauge = Gauge<f64, AtomicU64>;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MarketLabel {
    pub market: MarketId,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct QuoteLabels {
    pub market: MarketId,
    pub venue: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RejectionLabels {
    /// The rejection event's type in the public log, such as `order_rejected`.
    pub event: &'static str,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct SeverityLabel {
    pub severity: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DeliveryLabels {
    pub severity: &'static str,
    pub outcome: &'static str,
}

pub struct Metrics {
    registry: Registry,
    pub commands: Counter,
    pub batch_seconds: Histogram,
    pub queue_depth: Gauge,
    pub access_pending_deposits: Gauge,
    pub withdrawals_paused: Gauge,
    pub withdrawals_verdict_seq: Gauge,
    pub fills: Family<MarketLabel, Counter>,
    pub rejections: Family<RejectionLabels, Counter>,
    pub last_checkpoint_seconds: FloatGauge,
    pub house_nav: FloatGauge,
    pub house_gross_exposure: FloatGauge,
    pub house_net_exposure: Family<MarketLabel, FloatGauge>,
    pub market_open: Family<MarketLabel, Gauge>,
    pub market_priced: Family<MarketLabel, Gauge>,
    pub quote_timestamp_seconds: Family<QuoteLabels, FloatGauge>,
    pub alerts_dropped: Counter,
    pub alert_outbox_overflows: Counter,
    pub alert_deliveries: Family<DeliveryLabels, Counter>,
    pub alert_attempts_failed: Family<SeverityLabel, Counter>,
    pub alert_pages_pending: Family<SeverityLabel, Gauge>,
    pub tls_certificate_not_after: FloatGauge,
    pub tls_certificate_pending: Gauge,
}

impl Metrics {
    /// A fresh registry with every node metric registered under the `engine_` prefix, and every
    /// rejection and delivery series a node can produce created at zero, so a rate counts its first
    /// event.
    pub fn new() -> Self {
        let mut metrics = Self {
            registry: Registry::with_prefix("engine"),
            commands: Counter::default(),
            // 50µs to about 1.6s.
            batch_seconds: Histogram::new(exponential_buckets(0.000_05, 2.0, 16)),
            queue_depth: Gauge::default(),
            access_pending_deposits: Gauge::default(),
            withdrawals_paused: Gauge::default(),
            withdrawals_verdict_seq: Gauge::default(),
            fills: Family::default(),
            rejections: Family::default(),
            last_checkpoint_seconds: FloatGauge::default(),
            house_nav: FloatGauge::default(),
            house_gross_exposure: FloatGauge::default(),
            house_net_exposure: Family::default(),
            market_open: Family::default(),
            market_priced: Family::default(),
            quote_timestamp_seconds: Family::default(),
            alerts_dropped: Counter::default(),
            alert_outbox_overflows: Counter::default(),
            alert_deliveries: Family::default(),
            alert_attempts_failed: Family::default(),
            alert_pages_pending: Family::default(),
            tls_certificate_not_after: FloatGauge::default(),
            tls_certificate_pending: Gauge::default(),
        };
        let r = &mut metrics.registry;
        r.register(
            "alert_outbox_overflows",
            "Pages refused by durable outbox size bounds",
            metrics.alert_outbox_overflows.clone(),
        );
        r.register("commands", "Commands sequenced", metrics.commands.clone());
        r.register(
            "sequencer_batch_seconds",
            "Time from taking a batch's first request to its log flush and replies, for batches that sequence a command",
            metrics.batch_seconds.clone(),
        );
        r.register("sequencer_queue_depth", "Requests waiting for the sequencer", metrics.queue_depth.clone());
        r.register(
            "access_pending_deposits",
            "Deposits awaiting automatic source screening retry",
            metrics.access_pending_deposits.clone(),
        );
        r.register(
            "withdrawals_paused",
            "Whether the custody checkpoint gate pauses withdrawal approvals (1 paused, 0 healthy); paused until the \
             first reconciliation, while the finalized checkpoint's custody prefix is unproven, on a pending or \
             diverged comparison, and while the finalized vault pauses withdrawals or has no signer",
            metrics.withdrawals_paused.clone(),
        );
        r.register(
            "withdrawals_verdict_seq",
            "Sequence number of the finalized checkpoint behind the gate's latest collateral verdict",
            metrics.withdrawals_verdict_seq.clone(),
        );
        r.register("fills", "Fills, by market", metrics.fills.clone());
        r.register("rejections", "Rejection events, by event type and reason", metrics.rejections.clone());
        r.register(
            "last_checkpoint_timestamp_seconds",
            "Sequenced time of the command the last signed checkpoint covers",
            metrics.last_checkpoint_seconds.clone(),
        );
        r.register("house_nav", "House cash minus clients' unrealised PnL", metrics.house_nav.clone());
        r.register(
            "house_gross_exposure",
            "Sum over markets of the house's absolute net notional, in USD",
            metrics.house_gross_exposure.clone(),
        );
        r.register(
            "house_net_exposure",
            "The house's signed net notional in USD, by market",
            metrics.house_net_exposure.clone(),
        );
        r.register("market_open", "1 while the market's session is open", metrics.market_open.clone());
        r.register(
            "market_priced",
            "1 while the market has a usable index at sequenced time",
            metrics.market_priced.clone(),
        );
        r.register(
            "quote_timestamp_seconds",
            "Venue timestamp of the newest quote the engine holds, by market and venue, for the venues this node feeds; \
             the node's start time for one that has not quoted",
            metrics.quote_timestamp_seconds.clone(),
        );
        r.register(
            "alerts_dropped",
            "Alert lines dropped because the router's queue was full",
            metrics.alerts_dropped.clone(),
        );
        r.register(
            "alert_deliveries",
            "Webhook posts of alert pages, by severity and outcome",
            metrics.alert_deliveries.clone(),
        );
        r.register(
            "alert_attempts_failed",
            "Webhook posts of alert pages that failed, each retry counted, by severity",
            metrics.alert_attempts_failed.clone(),
        );
        r.register(
            "alert_pages_pending",
            "Alert pages queued for, or being posted to, their webhook, by severity",
            metrics.alert_pages_pending.clone(),
        );
        r.register(
            "tls_certificate_not_after_seconds",
            "Expiry, in Unix seconds, of the CA certificate the API serves; 0 while it serves only its self-signed one",
            metrics.tls_certificate_not_after.clone(),
        );
        r.register(
            "tls_certificate_pending",
            "1 while [api.tls.acme] is set and the CA has issued no certificate yet; 0 otherwise",
            metrics.tls_certificate_pending.clone(),
        );
        let typed = ["order_rejected", "withdrawal_rejected", "deposit_rejected", "staking_rejected"];
        let rejections = typed
            .iter()
            .flat_map(|event| REJECT_KINDS.map(|reason| (*event, reason)))
            .chain(DEPOSIT_REJECT_KINDS.map(|reason| ("custody_deposit_rejected", reason)))
            .chain(PRICES_REJECT_REASONS.map(|reason| ("prices_rejected", reason)))
            .chain([("admin_rejected", "other"), ("corporate_action_failed", "other")]);
        rejections.for_each(|(event, reason)| {
            let _ = metrics.rejections.get_or_create(&RejectionLabels { event, reason });
        });
        for severity in ["critical", "warning"] {
            let _ = metrics.alert_attempts_failed.get_or_create(&SeverityLabel { severity });
            let _ = metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity });
            for outcome in DELIVERY_OUTCOMES {
                let _ = metrics.alert_deliveries.get_or_create(&DeliveryLabels { severity, outcome });
            }
        }
        metrics
    }

    /// Counts fills and rejections among a command's events.
    pub fn record(&self, event: &Event) {
        let rejection = |event, reason| self.rejections.get_or_create(&RejectionLabels { event, reason }).inc();
        match event {
            Event::EligibilityRejected { reason, .. } => {
                rejection("eligibility_rejected", reject_kind(reason));
            }
            Event::DepositPending { .. } => {
                rejection("deposit_pending", "access");
            }
            Event::DepositHeld { .. } => {
                rejection("deposit_held", "access");
            }
            Event::Fill(fill) => {
                self.fills.get_or_create(&MarketLabel { market: fill.market }).inc();
            }
            Event::OrderRejected { reason, .. } => {
                rejection("order_rejected", reject_kind(reason));
            }
            Event::WithdrawalRejected { reason, .. } => {
                rejection("withdrawal_rejected", reject_kind(reason));
            }
            Event::DepositRejected { reason, .. } => {
                rejection("deposit_rejected", reject_kind(reason));
            }
            Event::StakingRejected { reason, .. } => {
                rejection("staking_rejected", reject_kind(reason));
            }
            Event::ElectionRejected { .. } => {
                rejection("election_rejected", "other");
            }
            Event::CustodyDepositRejected { reason, .. } => {
                rejection("custody_deposit_rejected", deposit_reject_kind(reason));
            }
            Event::PricesRejected { reason, .. } => {
                rejection("prices_rejected", prices_reject_reason(reason));
            }
            Event::AdminRejected { .. } => {
                rejection("admin_rejected", "other");
            }
            Event::CorporateActionFailed { .. } => {
                rejection("corporate_action_failed", "other");
            }
            _ => {}
        }
    }

    /// The registry in OpenMetrics text format.
    pub fn encode(&self) -> String {
        let mut text = String::new();
        // Writing into a `String` cannot fail.
        encode(&mut text, &self.registry).expect("metrics encode into a String");
        text
    }
}

/// `GET /metrics`, for the operator listener.
pub fn router(metrics: Arc<Metrics>) -> Router {
    Router::new().route("/metrics", get(scrape)).with_state(metrics)
}

/// Serves the current metrics.
async fn scrape(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    ([(CONTENT_TYPE, CONTENT_TYPE_OPENMETRICS)], metrics.encode())
}

/// A custody receipt rejection reason as the public log names it.
fn deposit_reject_kind(reason: &DepositReject) -> &'static str {
    match reason {
        DepositReject::ProtocolNotActive => "protocol_not_active",
        DepositReject::InvalidReceipt => "invalid_receipt",
        DepositReject::InvalidAmount => "invalid_amount",
        DepositReject::ArithmeticOverflow => "arithmetic_overflow",
        DepositReject::UnknownReceipt => "unknown_receipt",
        DepositReject::ConflictingReceipt => "conflicting_receipt",
        DepositReject::SettledReceipt => "settled_receipt",
    }
}

/// A rejection reason's `kind` in the public log.
fn reject_kind(reason: &Reject) -> &'static str {
    match reason {
        Reject::Access { .. } => "access",
        Reject::PayoutUnavailable => "payout_unavailable",
        Reject::UnknownMarket => "unknown_market",
        Reject::UnknownOrder => "unknown_order",
        Reject::InvalidOrder { .. } => "invalid_order",
        Reject::MarketClosed => "market_closed",
        Reject::ReduceOnly => "reduce_only",
        Reject::NoPrice { .. } => "no_price",
        Reject::NoFxPrice { .. } => "no_fx_price",
        Reject::Slippage { .. } => "slippage",
        Reject::InsufficientMargin { .. } => "insufficient_margin",
        Reject::InsufficientFunds { .. } => "insufficient_funds",
        Reject::BelowMinimumWithdrawal { .. } => "below_minimum_withdrawal",
        Reject::HouseCapacity => "house_capacity",
        Reject::CloseSpreadCeiling { .. } => "close_spread_ceiling",
        Reject::TooManyOrders { .. } => "too_many_orders",
        Reject::InvalidAmount => "invalid_amount",
        Reject::DuplicateReference => "duplicate_reference",
        Reject::CustodyReceiptRequired => "custody_receipt_required",
        Reject::MissingSignature => "missing_signature",
        Reject::InvalidSignature => "invalid_signature",
        Reject::InvalidNonce { .. } => "invalid_nonce",
        Reject::Expired { .. } => "expired",
        Reject::ExpiryTooFar { .. } => "expiry_too_far",
        Reject::StakingNotInitialised => "staking_not_initialised",
        Reject::InsufficientStake => "insufficient_stake",
    }
}

/// A price rejection's reason, read from the engine's text. Tests sequence each rejection through
/// the engine, so a changed text fails them rather than falling into `other`.
fn prices_reject_reason(reason: &str) -> &'static str {
    if reason == "unknown market" {
        "unknown_market"
    } else if reason.starts_with("malformed quote") {
        "malformed_quote"
    } else if reason.ends_with("ahead of the sequenced time") {
        "quote_ahead"
    } else if reason.ends_with("outside one open session") {
        "market_closed"
    } else if reason.ends_with("which the market does not take") {
        "contract_not_taken"
    } else {
        "other"
    }
}

/// A `Fixed` as a float, for display only.
pub fn to_f64(value: Fixed) -> f64 {
    value.raw() as f64 / Fixed::ONE.raw() as f64
}

#[cfg(test)]
mod tests;
