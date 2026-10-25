//! The engine state machine: `apply(sequenced command) -> events`.
//!
//! Single-threaded and deterministic. Time comes only from the sequencer's stamp. This file holds
//! the state, dispatch, time, sessions, and USD valuation; venue prices and marks live in
//! `marks.rs`, funds and financing in `funds.rs`, admin in `admin.rs`, stress levels in
//! `stress.rs`, client request auth in `auth.rs`, orders and pricing in `execution/`, close-outs in
//! `closeout.rs`, and markouts in `markouts.rs`.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::account::{Account, Position};
use crate::auth::{ClientAuth, Operator};
use crate::benchmark::BenchmarkHistory;
use crate::command::{AdminCommand, Command, Sequenced};
use crate::deposits::{CustodyCheckpoint, LegacyDeposit, RecordedDeposit};
use crate::emissions::{Emissions, PeriodResult, TokenUnits};
use crate::event::{Event, Reject};
use crate::fixed::Rounding;
use crate::market::MarketSpec;
use crate::merkle::state::{Changed, StateTree, is_zero};
use crate::params::ProtocolParams;
use crate::price::{Index, SourceQuote, compute_index};
use crate::protocol::ProtocolVersion;
pub use crate::stress::{MoveBucket, StressState};
use crate::tracked::{TrackedMap, TrackedSet};
use crate::types::{AccountId, MAX_STAMP_MS, MarketId, Millis, OrderId, Side};

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketState {
    pub spec: MarketSpec,
    pub quotes: BTreeMap<String, SourceQuote>,
    pub open: bool,
    pub reduce_only: bool,
    /// Last index mid marked while the market and its FX market had a price. Positions are marked
    /// here, including while the market is closed or its FX market has no price.
    pub mark: Option<Fixed>,
    /// Last index of the FX market that could convert this market's quote currency: what its
    /// positions are valued at in USD. `None` for a USD market, and left out of the encoding then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx_mark: Option<Fixed>,
    /// Sum of client position qty; the house holds the opposite.
    pub client_qty: Fixed,
    /// Sum of client `entry * qty`, so unrealised PnL is O(1) per market.
    pub client_cost: Fixed,
    pub stress: StressState,
    /// Resting orders in time priority.
    pub resting: BTreeMap<OrderId, AccountId>,
    /// From `V12`, the index the mark, stress, resting orders, and close-outs were last processed
    /// at, or `None` once the market cannot deal: closed, with no valid index, or with no FX price.
    /// Left out of the encoding while `None`, so roots before `V12` do not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processed_index: Option<Index>,
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

impl MarketState {
    /// A newly listed market: closed until its first session check, with no price or positions.
    pub(crate) fn new(spec: MarketSpec) -> Self {
        MarketState {
            spec,
            quotes: BTreeMap::new(),
            open: false,
            reduce_only: false,
            mark: None,
            fx_mark: None,
            client_qty: Fixed::ZERO,
            client_cost: Fixed::ZERO,
            stress: StressState::default(),
            resting: BTreeMap::new(),
            processed_index: None,
        }
    }

    /// `amount` in this market's quote currency, in USD at its FX mark, rounded as `rounding` says.
    pub fn usd_at_mark(&self, amount: Fixed, rounding: Rounding) -> Fixed {
        self.spec.quote_currency.to_usd(amount, self.fx_mark, rounding)
    }
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
        self.now = sequenced.ts;

        let mut events = Vec::new();
        // From `V19`, a period the sequenced time has passed is final before anything this command
        // does, so every effect of the command, market processing and rollovers included, belongs to
        // the next period.
        self.finalise_emissions(&mut events);
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
        let benchmark_batch = self.protocol_version >= ProtocolVersion::V18 && sequenced.cmd.is_benchmark_batch();
        let activating_dated_rates = sequenced.cmd.is_dated_rate_activation();
        if !benchmark_batch && !activating_dated_rates {
            self.run_rollovers(&mut events);
        }
        match &sequenced.cmd {
            // From `V12` the quotes are stored and processed above.
            Command::Prices { .. } if v12 => {}
            Command::Prices { market, quotes } => self.on_prices(*market, quotes, &mut events),
            Command::Tick => self.refresh_all_sessions(&mut events),
            Command::Deposit { account, amount, chain_ref } => self.deposit(*account, *amount, chain_ref, &mut events),
            Command::CustodyFinalised(checkpoint) => self.advance_custody_checkpoint(*checkpoint, &mut events),
            Command::CustodyDeposit(update) => self.update_deposit(update, &mut events),
            Command::Withdraw { account, amount, auth } => self.withdraw(*account, *amount, auth.as_ref(), &mut events),
            Command::HouseCapital { amount, chain_ref } => self.house_capital(*amount, chain_ref, &mut events),
            Command::PlaceOrder(req) => self.place_order(req, &mut events),
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
        }
        if benchmark_batch && events.contains(&Event::AdminApplied) {
            self.run_rollovers(&mut events);
        }
        self.refresh_all_stress(&sequenced.cmd, &mut events);
        // After the command, so a price arriving at the horizon is the one marked against.
        self.process_markouts();
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

    /// Whether a market takes no new risk: its own flag, or every market's.
    pub fn is_reduce_only(&self, market: &MarketState) -> bool {
        market.reduce_only || self.global_reduce_only
    }

    /// Every listed market, by id.
    pub fn markets(&self) -> &BTreeMap<MarketId, MarketState> {
        &self.markets
    }

    /// Every account that has ever been credited, by id.
    pub fn accounts(&self) -> &BTreeMap<AccountId, Account> {
        &self.accounts
    }

    /// Whether the custody reference `chain_ref` has been credited.
    pub fn is_credited(&self, chain_ref: &str) -> bool {
        self.credited_refs.contains(chain_ref)
    }

    /// The credited custody references that start with `prefix`, in order.
    pub fn credited_refs_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let from = (Bound::Included(prefix), Bound::Unbounded);
        self.credited_refs.range::<str, _>(from).map(String::as_str).take_while(move |r| r.starts_with(prefix))
    }

    /// House cash, before clients' unrealised PnL.
    pub fn house_cash(&self) -> Fixed {
        self.house_cash
    }

    /// House cash minus clients' unrealised PnL at current marks, in USD. Clients' PnL rounds up, so
    /// NAV, and every cap set from it, rounds down.
    pub fn house_nav(&self) -> Fixed {
        let client_upnl: Fixed = self
            .markets
            .values()
            .filter_map(|market| {
                let upnl = |mark: Fixed| mark.mul(market.client_qty) - market.client_cost;
                market.mark.map(|mark| market.usd_at_mark(upnl(mark), Rounding::Up))
            })
            .sum();
        self.house_cash - client_upnl
    }

    /// Sum over markets of the house's absolute net notional, in USD, each rounded up.
    pub fn house_gross_exposure(&self) -> Fixed {
        self.markets
            .values()
            .filter_map(|market| {
                market.mark.map(|mark| market.usd_at_mark(market.client_qty.abs().mul(mark), Rounding::Up))
            })
            .sum()
    }

    /// The cap on the house's absolute net notional in `spec`'s market at house NAV `nav`: its share
    /// of NAV, or from `V6` the larger of that and the market's floor.
    pub(crate) fn market_cap(&self, spec: &MarketSpec, nav: Fixed) -> Fixed {
        self.floored(nav.mul_bps(spec.house_cap_bps), spec.house_cap_floor)
    }

    /// The cap on the house's gross exposure across markets at house NAV `nav`: its share of NAV,
    /// or from `V6` the larger of that and the total floor.
    pub(crate) fn total_cap(&self, params: &ProtocolParams, nav: Fixed) -> Fixed {
        self.floored(nav.mul_bps(params.house_total_cap_bps), params.house_total_cap_floor)
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
        match compute_index(&fx.quotes, &fx.spec.price_rules, self.now) {
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
        let open = market.spec.schedule.is_open(now);
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

    /// `share`, or from `V6` the larger of `share` and `floor`.
    fn floored(&self, share: Fixed, floor: Fixed) -> Fixed {
        if self.protocol_version >= ProtocolVersion::V6 { share.max(floor) } else { share }
    }

    /// Refreshes every market's session, so opens and closes fire even without price traffic.
    pub(crate) fn refresh_all_sessions(&mut self, events: &mut Vec<Event>) {
        let ids: Vec<MarketId> = self.markets.keys().copied().collect();
        for market_id in ids {
            self.refresh_session(market_id, events);
        }
    }
}
