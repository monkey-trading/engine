//! Engine inputs. Everything that can change state is a [`Command`], and
//! every command is stamped and sequenced before the engine sees it. The
//! sequenced stream *is* the public event log; replaying it through this
//! crate reproduces the state exactly.

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::access::{AccessDeposit, AccessEvidence, AccessPolicy, EligibilityRequest};
use crate::auth::{ClientAuth, Operator, RequestAuth};
use crate::benchmark::BenchmarkBatch;
use crate::corporate::CorporateAction;
use crate::deposits::{CustodyCheckpoint, DepositReceipt, DepositUpdate, LegacyDeposit};
use crate::emissions::{EmissionSchedule, TokenUnits};
use crate::market::MarketSpec;
use crate::params::{NotionalWeighting, ProtocolParams};
use crate::price::SourceQuote;
use crate::protocol::ProtocolVersion;
use crate::staking::StakingConfig;
use crate::types::{AccountId, MarketId, Millis, OrderId, Side};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// The account's signed professional or eligible-client declaration, from V24, with the API's
    /// actual screening input.
    AttestEligibility {
        request: EligibilityRequest,
        evidence: AccessEvidence,
    },
    /// An authenticated order with the API's actual screening input.
    AccessOrder {
        request: OrderRequest,
        evidence: AccessEvidence,
    },
    /// An authenticated withdrawal with the API's actual screening input.
    AccessWithdraw {
        request: WithdrawalRequest,
        evidence: AccessEvidence,
        payout_available: bool,
    },
    /// Retries the screening of a pending deposit; held and credited references refuse it.
    RetryDepositScreening {
        chain_ref: String,
        evidence: AccessEvidence,
    },
    /// A mock or historical custody deposit, including its funding address and screening input.
    AccessDeposit {
        deposit: AccessDeposit,
        evidence: AccessEvidence,
    },
    /// A new quorum-proven custody receipt with its vault caller and screening input, from `V24`;
    /// it is credited, left pending, or held uncredited.
    AccessCustodyDeposit {
        receipt: DepositReceipt,
        payer: Option<AccountId>,
        evidence: AccessEvidence,
    },
    /// An atomic custody replacement screened against its canonical payer, from `V26`.
    AccessCustodyReplacement {
        update: Box<DepositUpdate>,
        payer: Option<AccountId>,
        evidence: AccessEvidence,
    },
    /// Venue quotes from the feed handlers.
    Prices {
        market: MarketId,
        quotes: Vec<SourceQuote>,
    },
    /// Advances time when nothing else is happening, so session changes and
    /// rollovers fire on schedule.
    Tick,
    /// Legacy or mock credit, at most once per reference. The old configured tag is not a finality
    /// proof; receipt-finality rules require exact custody receipts for EVM credits.
    Deposit {
        account: AccountId,
        amount: Fixed,
        chain_ref: String,
    },
    /// A quorum-proven custody receipt transition, from V20.
    CustodyDeposit(Box<DepositUpdate>),
    /// Finalised scan progress, recorded after its receipts have settled.
    CustodyFinalised(CustodyCheckpoint),
    /// A trader asks to withdraw. If approved, the enclave signs a
    /// withdrawal the vault contract will honour.
    Withdraw {
        account: AccountId,
        amount: Fixed,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<RequestAuth>,
    },
    /// House capital in or out, applied at most once per `chain_ref`. Equity issuance against it
    /// is not designed yet; for now this only moves house NAV.
    HouseCapital {
        amount: Fixed,
        chain_ref: String,
    },
    PlaceOrder(OrderRequest),
    CancelOrder {
        account: AccountId,
        order_id: OrderId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<RequestAuth>,
    },
    Admin(AdminCommand),
    /// An admin command the operator signed, from `V8`. From `V8`, an unsigned `Admin` operator
    /// command is refused once any deposit or house capital is credited. Boxed: admin commands are
    /// rare, and inline this one would grow every command.
    SignedAdmin(Box<AdminRequest>),
    /// From `V27`, the last fully verified finalized staking block, including empty ranges.
    StakingProgress {
        block: u64,
        block_hash: B256,
    },
    /// From `V25`, a staking contract's `Staked` event, finalised: `amount` of MONKEY base units
    /// staked for `account`, credited once under `chain_ref`, in the finalised Ethereum `block`
    /// the event was mirrored from (see `Engine`'s staking frontier).
    Stake {
        account: AccountId,
        amount: TokenUnits,
        #[serde(default)]
        block: u64,
        chain_ref: String,
    },
    /// From `V25`, a staking contract's `UnstakeCompleted` event, which fires only once the
    /// unstake cooldown has passed: `amount` of MONKEY base units unstaked for `account`, credited
    /// once under `chain_ref`, in the finalised Ethereum `block` the event was mirrored from.
    Unstake {
        account: AccountId,
        amount: TokenUnits,
        #[serde(default)]
        block: u64,
        chain_ref: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OrderKind {
    /// Fill now at the live price. `quoted` is the price the client saw.
    /// Quotes are indicative, not firm: the fill happens at the live price,
    /// and the order is declined only if that is more than
    /// `max_slippage_bps` worse than `quoted`. Better prices pass through.
    Market {
        quoted: Fixed,
        #[serde(default)]
        max_slippage_bps: Option<i64>,
    },
    /// Rests until the live price is at `price` or better, then fills at the
    /// live price.
    Limit { price: Fixed },
    /// Rests until the live price trades through `price`, then fills at the
    /// live price, gap included. Not guaranteed.
    Stop { price: Fixed },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AdminCommand {
    /// Node-only activation with legacy EVM credits reconstructed from the verified public log.
    ActivateCustodyFinality {
        version: ProtocolVersion,
        legacy_deposits: Vec<LegacyDeposit>,
    },
    /// An immutable corporate instruction, submitted before the previous venue close.
    ScheduleCorporateAction(CorporateAction),
    /// Records the access source and freshness bound once, before any screened action can pass.
    SetAccessPolicy(AccessPolicy),
    SetParams(ProtocolParams),
    /// Lists a new market or replaces an existing market's spec.
    UpsertMarket(MarketSpec),
    SetBenchmarkRate {
        benchmark: String,
        annual_bps: i64,
    },
    /// An atomic publisher-history snapshot, installed before missed rollovers are charged.
    SetBenchmarkRates(BenchmarkBatch),
    /// The operator's stress floor for one market, or every market if
    /// `market` is `None`. The effective level is the higher of this and the
    /// automatic level, so the operator can raise stress but never suppress it.
    SetStressFloor {
        market: Option<MarketId>,
        level: usize,
    },
    /// Stops new risk in one market, or from `V8` in every market, listed now or later, if
    /// `market` is `None`. Reducing and closing trades stay allowed in every mode.
    SetReduceOnly {
        market: Option<MarketId>,
        reduce_only: bool,
    },
    /// Records how client requests are authorised from `V3`. A log records it once.
    SetClientAuth(ClientAuth),
    /// Records the operator whose signed admin commands apply from `V8`. A log records it once.
    SetOperator(Operator),
    /// Gives parameters set before `V11`, which do not weigh markouts by notional, the node's
    /// weighting, so that `V11` can activate over them.
    SetToxicityNotional(NotionalWeighting),
    /// Applies `version`'s rules from this seq on. Versions only move forward.
    ActivateProtocol {
        version: ProtocolVersion,
    },
    /// From `V19`, records the MONKEY emission schedule: when period 0 starts, no earlier than this
    /// command, and its budget. A log records it once, and only signed by the operator.
    InitEmissions(EmissionSchedule),
    /// From `V25`, records the MONKEY staking config: the liquidity floor, the epoch length, when
    /// epoch 0 starts, and the unstake cooldown the staking contract enforces. A log records it
    /// once.
    InitStaking(StakingConfig),
}

/// An admin command with the operator's signature, nonce, and expiry, as `engine admin` signs and
/// posts it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRequest {
    pub command: AdminCommand,
    pub auth: RequestAuth,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sequenced {
    pub seq: u64,
    pub ts: Millis,
    pub cmd: Command,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderRequest {
    pub account: AccountId,
    /// Client-chosen id, echoed back so clients can match responses.
    pub client_id: u64,
    pub market: MarketId,
    pub side: Side,
    pub qty: Fixed,
    pub kind: OrderKind,
    /// Only reduce an existing position, never open or flip it.
    #[serde(default)]
    pub reduce_only: bool,
    /// The account's signature, nonce, and expiry, where the log's rules require them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<RequestAuth>,
}

/// A client's request to cancel a resting order, as it signs and posts it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub account: AccountId,
    pub order_id: OrderId,
    /// The account's signature, nonce, and expiry, where the log's rules require them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<RequestAuth>,
}

/// A client's request to withdraw, as it signs and posts it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WithdrawalRequest {
    pub account: AccountId,
    pub amount: Fixed,
    /// The account's signature, nonce, and expiry, where the log's rules require them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<RequestAuth>,
}

impl AdminCommand {
    /// Whether this is a dated publisher snapshot.
    pub fn is_benchmark_snapshot(&self) -> bool {
        matches!(self, Self::SetBenchmarkRates(_))
    }

    /// Whether the operator may sign this command: listings, parameters, benchmark rates, stress
    /// floors, reduce-only, the emission schedule, corporate actions, and the staking config. The
    /// node alone sequences the others, from its config and build.
    pub fn is_operator_command(&self) -> bool {
        match self {
            AdminCommand::SetParams(_)
            | AdminCommand::UpsertMarket(_)
            | AdminCommand::SetBenchmarkRate { .. }
            | AdminCommand::SetBenchmarkRates(_)
            | AdminCommand::SetStressFloor { .. }
            | AdminCommand::SetReduceOnly { .. }
            | AdminCommand::InitEmissions(_)
            | AdminCommand::ScheduleCorporateAction(_)
            | AdminCommand::InitStaking(_) => true,
            AdminCommand::SetClientAuth(_)
            | AdminCommand::SetAccessPolicy(_)
            | AdminCommand::SetOperator(_)
            | AdminCommand::SetToxicityNotional(_)
            | AdminCommand::ActivateCustodyFinality { .. }
            | AdminCommand::ActivateProtocol { .. } => false,
        }
    }
}

impl Command {
    /// Targets the activation that validates staking cash reserves before closing an overdue epoch.
    pub(crate) fn changes_staking_reserves(&self, current: ProtocolVersion) -> bool {
        let admin = match self {
            Self::Admin(admin) => admin,
            Self::SignedAdmin(request) => &request.command,
            _ => return false,
        };
        let target = match admin {
            AdminCommand::ActivateProtocol { version } | AdminCommand::ActivateCustodyFinality { version, .. } => {
                *version
            }
            _ => return false,
        };
        (current < ProtocolVersion::V28 && target >= ProtocolVersion::V28)
            || (current < ProtocolVersion::V29 && target >= ProtocolVersion::V29)
            || (current < ProtocolVersion::V30 && target >= ProtocolVersion::V30)
    }

    /// Whether this node-only activation must precede dated benchmark catch-up.
    pub fn is_dated_rate_activation(&self) -> bool {
        matches!(self,
            Self::Admin(AdminCommand::ActivateProtocol { version }
                | AdminCommand::ActivateCustodyFinality { version, .. }) if *version >= ProtocolVersion::V18)
    }

    /// Whether this carries an atomic dated benchmark snapshot, signed or during genesis.
    pub fn is_benchmark_batch(&self) -> bool {
        matches!(self, Self::Admin(AdminCommand::SetBenchmarkRates(_)))
            || matches!(self, Self::SignedAdmin(req) if matches!(req.command, AdminCommand::SetBenchmarkRates(_)))
    }
}
