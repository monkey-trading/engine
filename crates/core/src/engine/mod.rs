//! The engine state machine: `apply(sequenced command) -> events`.
//!
//! Single-threaded and deterministic. Time comes only from the sequencer's stamp. This file holds
//! the state, dispatch, time, sessions, and USD valuation; house NAV and the exposure caps set
//! from it live in `house.rs`; per-market state lives in `market.rs`,
//! venue prices and marks in `marks.rs`, funds and financing in `funds.rs`, admin in `admin.rs`,
//! stress levels in `stress.rs`, client request auth in `auth.rs`, orders and pricing in
//! `execution/`, close-outs in `closeout.rs`, markouts in `markouts.rs`, reserved FIFO
//! withdrawals in `withdrawal.rs`, and MONKEY staking and USDC distributions in `staking.rs`.

use std::collections::{BTreeMap, VecDeque};

mod house;
mod market;

pub use market::MarketState;

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::access::{AccessDeposit, AccessPolicy, CreditScreening, EligibilityClass, HeldDeposit};
use crate::account::{Account, Position};
use crate::auth::{ClientAuth, Operator};
use crate::benchmark::BenchmarkHistory;
use crate::command::{AdminCommand, Command, Sequenced};
use crate::corporate::CorporateEntitlement;
use crate::deposits::{CustodyCheckpoint, LegacyDeposit, RecordedDeposit};
use crate::emissions::{EmissionLedger, Emissions, PeriodResult, TokenUnits};
use crate::event::{Event, Reject};
use crate::fixed::Rounding;
use crate::merkle::state::{Changed, StateTree, is_zero};
use crate::params::ProtocolParams;
use crate::protocol::ProtocolVersion;
use crate::staking::{Staking, UsdcUnits};
pub use crate::stress::{MoveBucket, StressState};
use crate::tracked::{TrackedMap, TrackedSet};
use crate::types::{AccountId, MAX_STAMP_MS, MarketId, Millis, OrderId, Side};
use crate::withdrawal::{QueuedWithdrawal, WithdrawalApproval};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SequenceError {
    #[error("expected seq {expected}, got {got}")]
    Gap { expected: u64, got: u64 },
    #[error("time went backwards: {last} -> {got}")]
    TimeReversed { last: Millis, got: Millis },
    #[error("stamp {got} at seq {seq} is past the end of 9999, the latest the engine accepts")]
    StampOutOfRange { seq: u64, got: Millis },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Engine {
    pub(crate) params: Option<ProtocolParams>,
    pub(crate) markets: TrackedMap<MarketId, MarketState>,
    pub(crate) accounts: TrackedMap<AccountId, Account>,
    /// House cash. NAV is this minus clients' unrealised PnL.
    pub(crate) house_cash: Fixed,
    /// Annual benchmark rates in bps, keyed by name ("SOFR", ...).
    pub(crate) benchmarks: BTreeMap<String, i64>,
    /// Custody references already credited (deposits and house capital), so none applies twice.
    pub(crate) credited_refs: TrackedSet<String>,
    /// Staking mirror references already credited, a namespace of their own: the accounting's
    /// credited-reference scan counts `credited_refs` as vault deposits and house capital, and a
    /// staking mirror moves no USDC, so its references must not enter that count.
    #[serde(default, skip_serializing_if = "TrackedSet::is_empty")]
    pub(crate) staking_refs: TrackedSet<String>,
    pub(crate) global_stress_floor: usize,
    pub(crate) next_order_id: OrderId,
    pub(crate) next_rollover: Option<Millis>,
    pub(crate) markouts: VecDeque<Markout>,
    pub(crate) last_seq: Option<u64>,
    pub(crate) now: Millis,
    // The fields below are left out of the encoding while they hold their defaults, so a log that
    // never activates a protocol version keeps the roots it was written with.
    /// The rules in force.
    #[serde(default, skip_serializing_if = "ProtocolVersion::is_v2")]
    pub(crate) protocol_version: ProtocolVersion,
    /// How client requests are authorised from `V3`; recorded once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_auth: Option<ClientAuth>,
    /// Each signer's next request nonce, the accounts' and the operator's; a signer missing here
    /// is at 0.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) request_nonces: TrackedMap<AccountId, u64>,
    /// Who signs admin commands from `V8`; recorded once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) operator: Option<Operator>,
    /// Every market is reduce-only, whatever its own flag says.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) global_reduce_only: bool,
    /// The state tree's index for the front markout: 0 before `V9`, then one more per markout.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) markout_base: u64,
    /// Dated annual rates from V18; omitted while empty to preserve older roots.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) benchmark_history: BenchmarkHistory,
    /// MONKEY emissions from `V19`, once the operator's signed initialisation records the schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) emissions: Option<Emissions>,
    /// Each account's realised trading result in the open emission period, until it is finalised.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) period_results: TrackedMap<AccountId, PeriodResult>,
    /// Each account's cumulative MONKEY entitlement.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) entitlements: TrackedMap<AccountId, TokenUnits>,
    /// Pending instructions, including captured previous-close quantities and failed instructions.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) corporate_actions: TrackedMap<String, CorporateEntitlement>,
    /// Accepted corporate identities, retained after completion to reject redelivery.
    #[serde(default, skip_serializing_if = "TrackedSet::is_empty")]
    pub(crate) corporate_refs: TrackedSet<String>,
    /// From `V32`, each account's emission ledger: the loss weight it earned that no write-off has
    /// been charged against, and the write-off debt later loss weight pays down.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) emission_ledgers: TrackedMap<AccountId, EmissionLedger>,
    /// Finalised custody scan progress is sequenced, never recovered from a host cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) custody_checkpoint: Option<CustodyCheckpoint>,
    /// Exact receipts and their lifecycle; each has its own state-tree leaf.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) deposits: TrackedMap<String, RecordedDeposit>,
    /// The latest receipt for each transaction-local deposit, for constant-time duplicate checks.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) deposit_transactions: TrackedMap<String, String>,
    /// Legacy EVM principal awaiting an exact canonical receipt, already credited in old rules.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) legacy_deposits: TrackedMap<String, LegacyDeposit>,
    /// House principal awaiting Ethereum finality.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub(crate) unfinalised_house_deposits: Fixed,
    /// Unapproved physical payout capacity from V22; initialized once from conserved cash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) withdrawal_cash: Option<Fixed>,
    /// FIFO reservations keyed by the sequence that requested them.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) withdrawal_queue: TrackedMap<u64, QueuedWithdrawal>,
    /// The next contiguous enqueue ticket; completed requests never reuse their ticket.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) withdrawal_next_ticket: u64,
    /// Immutable payouts keyed by their account/nonce approval leaf key.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) withdrawal_approvals: TrackedMap<B256, WithdrawalApproval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) staking: Option<Staking>,
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) staked: TrackedMap<AccountId, TokenUnits>,
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) distributions: TrackedMap<AccountId, UsdcUnits>,

    /// The screening source and freshness bound, recorded once from `V24`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) access_policy: Option<AccessPolicy>,
    /// Each account's recorded eligibility declaration.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) eligibility: TrackedMap<AccountId, EligibilityClass>,
    /// Deposits held uncredited for good, by chain reference.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) held_deposits: TrackedMap<String, HeldDeposit>,
    /// Deposits awaiting a declaration, a classification or a working source, by chain reference.
    #[serde(default, skip_serializing_if = "TrackedMap::is_empty")]
    pub(crate) pending_deposits: TrackedMap<String, AccessDeposit>,
    // Not state and not encoded; an engine rebuilt without them hashes every leaf at its first root.
    /// Markouts, positions, and resting orders changed since the last state root.
    #[serde(skip)]
    pub(crate) changed: Changed,
    /// Derived account and trigger indexes; rebuilt once after deserialization.
    #[serde(skip)]
    pub(crate) hot_indexes: Option<crate::hot_indexes::HotIndexes>,
    /// The state tree as of the last root under `V9`; absent until then.
    #[serde(skip)]
    pub(crate) tree: Option<StateTree>,
    /// The stamp of the command being applied. `now` rewinds to a boundary during corporate
    /// catch-up; this does not.
    #[serde(skip)]
    pub(crate) command_ms: Millis,
}

/// A fill waiting to be marked against the index once its horizon has passed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Markout {
    pub due: Millis,
    pub account: AccountId,
    pub market: MarketId,
    pub side: Side,
    pub price: Fixed,
    /// From `V11`, the fill's notional, which weighs its markout. Left out of the encoding while
    /// zero, as it is for fills booked before `V11`.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub notional: Fixed,
}

impl Engine {
    /// An empty engine, before genesis.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one sequenced command and returns the events it produced. A refused command is an
    /// event, not an error; an error means the sequencer broke its own contract.
    /// Error cases:
    /// - `seq` is not exactly one more than the last applied (or 0 for the first)
    /// - `ts` is earlier than the last applied
    /// - `ts` is later than the end of 9999, the latest stamp the engine accepts
    pub fn apply(&mut self, sequenced: &Sequenced) -> Result<Vec<Event>, SequenceError> {
        let expected = self.last_seq.map_or(0, |n| n + 1);
        if sequenced.seq != expected {
            return Err(SequenceError::Gap { expected, got: sequenced.seq });
        }
        if sequenced.ts < self.now {
            return Err(SequenceError::TimeReversed { last: self.now, got: sequenced.ts });
        }
        if sequenced.ts > MAX_STAMP_MS {
            return Err(SequenceError::StampOutOfRange { seq: sequenced.seq, got: sequenced.ts });
        }
        self.ensure_hot_indexes();
        self.last_seq = Some(sequenced.seq);
        let last_now = self.now;
        self.now = sequenced.ts;
        self.command_ms = sequenced.ts;

        let mut events = Vec::new();
        // From `V21`, a quote held across a closure could never count under the hours rules, so it
        // drops before anything this command does: catch-up at an intermediate boundary, a close-out
        // check, or an early outcome for a rejected batch or activation must never act on it.
        if self.protocol_version >= ProtocolVersion::V21 {
            self.drop_quotes_across_closures(last_now);
        }
        // From `V19`, a period the sequenced time has passed is final before anything this command
        // does, so every effect of the command, market processing and rollovers included, belongs to
        // the next period.
        self.finalise_emissions(&mut events);
        let staking_activation = sequenced.cmd.changes_staking_reserves(self.protocol_version);
        if !staking_activation {
            self.finalise_staking(&mut events);
        }
        let corporate = self.protocol_version >= ProtocolVersion::V23;
        let mut corporate_margin = false;
        let mut early_outcome = None;
        if corporate {
            let activation =
                matches!(sequenced.cmd, Command::Admin(AdminCommand::ActivateProtocol { .. })) || staking_activation;
            // Install authenticated dated history before catch-up, without letting today's
            // incoming quote price any earlier rollover or corporate boundary.
            if sequenced.cmd.is_benchmark_batch() || activation {
                let result = match &sequenced.cmd {
                    Command::Admin(cmd) => self.unsigned_admin(cmd, &mut events),
                    Command::SignedAdmin(req) => self.signed_admin(req, &mut events),
                    _ => unreachable!("benchmark batches and activations are admin commands"),
                };
                if staking_activation && result.is_ok() {
                    self.finalise_staking(&mut events);
                }
                early_outcome = Some(match result {
                    Ok(()) => Event::AdminApplied,
                    Err(reason) => Event::AdminRejected { reason },
                });
            }
            // An activation, like a rejected benchmark batch, leaves every missed rollover and due
            // corporate boundary to the next admitted command, which the node holds until the
            // dated snapshot is in.
            if activation || matches!(early_outcome, Some(Event::AdminRejected { .. })) {
                events.extend(early_outcome);
                return Ok(events);
            }
            // From `V33` a gap of more than `MAX_CATCH_UP_BOUNDARIES` boundaries defers the command,
            // reporting the outcome of a benchmark batch already applied above.
            let Some(margin) = self.run_corporate_timeline(last_now, sequenced.ts, &mut events) else {
                events.extend(early_outcome);
                return Ok(events);
            };
            corporate_margin = margin;
        }
        // From `V12`, a price command's quotes are stored first, then every market is brought to the
        // sequenced time, so no fill or check, a rollover's included, acts on an index the market
        // does not have at that time, or before it is processed.
        let v12 = self.protocol_version >= ProtocolVersion::V12;
        if v12 {
            let priced = match &sequenced.cmd {
                Command::Prices { market, quotes } => {
                    self.store_quotes(*market, quotes, &mut events).then_some(*market)
                }
                _ => None,
            };
            self.advance_markets(priced, &mut events);
        }
        if corporate {
            if corporate_margin {
                self.check_all_closeouts(&mut events);
            }
            if self.next_rollover == Some(self.now) {
                self.run_one_rollover(&mut events);
            }
        }
        let benchmark_batch = self.protocol_version >= ProtocolVersion::V18 && sequenced.cmd.is_benchmark_batch();
        let activating_dated_rates = sequenced.cmd.is_dated_rate_activation();
        if !corporate && !benchmark_batch && !activating_dated_rates {
            self.run_rollovers(&mut events);
        }
        if self.access_dispatch(&sequenced.cmd, &mut events) {
            // The access-screened commands and the V24 raw-funding wraps are handled.
        } else {
            match &sequenced.cmd {
                _ if early_outcome.is_some() => events.extend(early_outcome),
                // From `V12` the quotes are stored and processed above.
                Command::Prices { .. } if v12 => {}
                Command::Prices { market, quotes } => self.on_prices(*market, quotes, &mut events),
                Command::Tick => self.refresh_all_sessions(&mut events),
                Command::Deposit { account, amount, chain_ref } => {
                    self.deposit(*account, *amount, chain_ref, &mut events)
                }
                Command::CustodyFinalised(checkpoint) => self.advance_custody_checkpoint(*checkpoint, &mut events),
                Command::CustodyDeposit(update) => self.update_deposit(update, CreditScreening::default(), &mut events),
                Command::Withdraw { account, amount, auth } => {
                    self.withdraw(*account, *amount, auth.as_ref(), None, &mut events)
                }
                Command::HouseCapital { amount, chain_ref } => self.house_capital(*amount, chain_ref, &mut events),
                Command::PlaceOrder(req) => self.place_order(req, None, &mut events),
                Command::CancelOrder { account, order_id, auth } => {
                    self.cancel_order(*account, *order_id, auth.as_ref(), &mut events)
                }
                Command::Admin(cmd) => match self.unsigned_admin(cmd, &mut events) {
                    Ok(()) => events.push(Event::AdminApplied),
                    Err(reason) => events.push(Event::AdminRejected { reason }),
                },
                Command::SignedAdmin(req) => match self.signed_admin(req, &mut events) {
                    Ok(()) => events.push(Event::AdminApplied),
                    Err(reason) => events.push(Event::AdminRejected { reason }),
                },
                Command::Stake { account, amount, block, chain_ref } => {
                    self.stake(*account, *amount, *block, chain_ref, &mut events)
                }
                Command::StakingProgress { block, block_hash } => {
                    self.staking_progress(*block, *block_hash, &mut events)
                }
                Command::Unstake { account, amount, block, chain_ref } => {
                    self.unstake(*account, *amount, *block, chain_ref, &mut events)
                }
                // The access variants always dispatch to `access_dispatch` first.
                _ => unreachable!("access command dispatched above"),
            }
        }
        if !corporate && benchmark_batch && events.contains(&Event::AdminApplied) {
            self.run_rollovers(&mut events);
        }
        self.refresh_all_stress(&sequenced.cmd, &mut events);
        // After the command, so a price arriving at the horizon is the one marked against.
        self.process_markouts();
        if self.protocol_version >= ProtocolVersion::V22 {
            self.process_withdrawal_queue(&mut events);
        }
        Ok(events)
    }

    /// Seq of the last applied command, or `None` before genesis.
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Sequenced time of the last applied command.
    pub fn now(&self) -> Millis {
        self.now
    }

    /// The protocol parameters, once set.
    pub fn params(&self) -> Option<&ProtocolParams> {
        self.params.as_ref()
    }

    /// The protocol version in force.
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    /// How client requests are authorised from `V3`, once recorded.
    pub fn client_auth(&self) -> Option<&ClientAuth> {
        self.client_auth.as_ref()
    }

    /// The nonce `account`'s next signed request must carry; for the operator's address, its next
    /// admin command's.
    pub fn next_request_nonce(&self, account: AccountId) -> u64 {
        self.request_nonces.get(&account).copied().unwrap_or(0)
    }

    /// Who signs admin commands from `V8`, once recorded.
    pub fn operator(&self) -> Option<&Operator> {
        self.operator.as_ref()
    }

    /// Every listed market, by id.
    pub fn markets(&self) -> &BTreeMap<MarketId, MarketState> {
        &self.markets
    }

    /// Every account that has ever been credited, by id.
    pub fn accounts(&self) -> &BTreeMap<AccountId, Account> {
        &self.accounts
    }

    /// (equity, margin requirement) in USD at current marks, optionally overriding one market's mark
    /// and FX index.
    pub fn equity_and_margin(
        &self,
        acct: &Account,
        mark_override: Option<(MarketId, Fixed, Option<Fixed>)>,
    ) -> (Fixed, Fixed) {
        self.equity_and_margin_at(acct, |market_id, market, pos| match mark_override {
            Some((id, mid, fx)) if id == market_id => (mid, fx),
            _ => (market.mark.unwrap_or(pos.entry), market.fx_mark),
        })
    }

    /// (equity, margin requirement) in USD with each position valued at the price and FX index
    /// `price_of(market id, market, position)` gives: PnL converts rounded down and notional rounded
    /// up. From `V10` the requirement rounds up, from each position's notional to each band's charge.
    pub(crate) fn equity_and_margin_at(
        &self,
        acct: &Account,
        price_of: impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>),
    ) -> (Fixed, Fixed) {
        acct.positions
            .iter()
            .filter_map(|(market_id, pos)| self.markets.get(market_id).map(|market| (*market_id, pos, market)))
            .fold((acct.balance, Fixed::ZERO), |(equity, margin), (market_id, pos, market)| {
                let (price, fx) = price_of(market_id, market, pos);
                let usd = |amount, rounding| market.spec.quote_currency.to_usd(amount, fx, rounding);
                let notional = usd(pos.qty.abs().mul_rounded(price, self.charge_rounding()), Rounding::Up);
                let pnl = usd(pos.unrealised(price, self.protocol_version), Rounding::Down);
                (equity + pnl, margin + market.spec.margin_for(notional, self.charge_rounding()))
            })
    }

    /// The FX index a fill in `market_id` converts at now: its FX market's live index, or `None` for
    /// a USD market.
    /// Error cases:
    /// - the FX market's schedule is closed at the sequenced time, or it has no valid index, or one
    ///   that cannot convert the currency
    /// - the market has no FX mark yet, so a position in it could not be valued
    pub fn live_fx(&self, market_id: MarketId) -> Result<Option<Fixed>, Reject> {
        let market = &self.markets[&market_id];
        let currency = market.spec.quote_currency;
        let Some(fx_market) = currency.fx_market() else { return Ok(None) };
        // Listing checks the FX market is listed, and no market is ever removed.
        let fx = self.markets.get(&fx_market).expect("an FX market is listed before markets that use it");
        let no_price = Reject::NoFxPrice { fx_market };
        // The schedule, not the stored flag, which only the FX market's own commands refresh.
        if !fx.spec.schedule.is_open(self.now) || market.fx_mark.is_none() {
            return Err(no_price);
        }
        match fx.index(self.now) {
            Ok(index) if currency.converts_at(index.mid) => Ok(Some(index.mid)),
            _ => Err(no_price),
        }
    }

    /// Updates a market's open flag from its schedule, emitting the transition. Before `V17` an
    /// open cleared the market's move window; from `V17` the window survives the halt, so prices
    /// observed within it before the halt still count after the reopening.
    pub(crate) fn refresh_session(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        let now = self.now;
        let Some(market) = self.markets.get(&market_id) else { return };
        let open = market.spec.schedule.is_open(now) && !market.corporate_action_blocked;
        if open == market.open {
            return;
        }
        let market = self.markets.get_mut(&market_id).expect("found above");
        market.open = open;
        if open {
            if self.protocol_version < ProtocolVersion::V17 {
                market.stress.move_ref = None;
                market.stress.move_window.clear();
            }
            events.push(Event::MarketOpened { market: market_id });
        } else {
            events.push(Event::MarketClosed { market: market_id });
        }
    }
}
