//! Operator commands.
//!
//! Every admin action is a sequenced command, so the public log shows each parameter change, each
//! listing, and each stress override. From `V8` the operator signs its commands (listings,
//! parameters, benchmark rates, stress floors, and reduce-only) once anything is credited, and the
//! log carries each signature; from `V19` the emission schedule applies only signed, credited or
//! not. Client auth, the operator, and protocol activation stay commands only the node sequences.
//! The operator can raise a market's stress level but never push it below what the automatic
//! triggers set, and from `V8` neither a reduce-only mode nor a spread stops a trade that reduces
//! or closes a position in an open market with a price.

use crate::command::{AdminCommand, AdminRequest};
use crate::deposits::legacy_chain_reference;
use crate::engine::{Engine, MarketState};
use crate::event::{Event, Reject};
use crate::futures::FuturesState;
use crate::market::MarketSpec;
use crate::params::{DEFAULT_MAX_RESTING_ORDERS, MAX_BENCHMARK_BPS};
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

impl Engine {
    /// Applies an operator command. From `V6`, new parameters check every account for close-out, as
    /// the close-out level may rise, and a listing checks every account holding the market, as its
    /// margin table may rise. From `V7`, a listing of a market others convert at also marks it and
    /// them as a price update does. Activating `V13` turns the absolute divergence triggers in force
    /// into shares of the widest listed bound; a market already listed with a zero bound reaches a
    /// share above zero only once relisted with a positive one. From `V16`, parameters that set a
    /// smaller toxicity memory clamp every account's toxicity history to it, so the next markout
    /// already weighs no more history than the memory.
    /// Error cases:
    /// - `SetParams` with parameters that fail `ProtocolParams::validate_for` the version in force,
    ///   from `V8` that fail `ProtocolParams::check_closing_spread`, from `V11` that fail
    ///   `ProtocolParams::check_notional_weighting`, from `V13` with a divergence share a listed
    ///   market can never reach, or with a resting-order cap other than the default before `V14`
    /// - `UpsertMarket` with a spec that fails `MarketSpec::validate` or `check_quote_currency`, with
    ///   dated closures or special sessions before `V4`, from `V13` with a bound no divergence share
    ///   in force can be reached at, from `V14`, once any benchmark has a rate, that lists a
    ///   market, or moves one, onto a benchmark with none, with futures contracts before `V21`, or
    ///   that `MarketState::relisted` refuses for a listed market
    /// - `SetBenchmarkRate` outside ±10000 bps a year
    /// - `SetStressFloor` before parameters are set, at a level that does not exist, or for an
    ///   unknown market
    /// - `SetReduceOnly` for an unknown market, or for every market before `V8`
    /// - `SetClientAuth` once client auth is recorded
    /// - `SetOperator` once the operator is recorded
    /// - `ActivateProtocol` for a version no newer than the one in force, for `V3` or later before
    ///   client auth is recorded, or for `V8` or later before the operator is recorded or while the
    ///   parameters in force fail `ProtocolParams::check_closing_spread`, or for `V11` or later while
    ///   they fail `ProtocolParams::check_notional_weighting`
    /// - `SetToxicityNotional` before parameters are set, over parameters that already weigh markouts
    ///   by notional, or with weighting that fails `NotionalWeighting::validate`
    /// - `InitEmissions` before `V19`, once emissions are initialised, or with a schedule that starts
    ///   before the sequenced time or has a budget out of range
    /// - `ScheduleCorporateAction` with any error case of `Engine::schedule_corporate_action`
    /// - `UpsertMarket` changing the calendar, asset class, quote currency or symbol of a market
    ///   with a pending corporate action
    /// - `InitStaking` before `V25`, once staking is initialised, or with a config that fails its
    ///   checks at the sequenced time (see `StakingConfig::validate`)
    /// - from `V35`, `SetParams` that changes the profit fee or the close-out level outside ±50% of
    ///   its launch value, or without a notice whose time has come (see
    ///   `Engine::check_rate_changes`); `UpsertMarket` whose price rules leave a market with open
    ///   positions unpriced and not reduce-only (see `Engine::check_priceable`)
    /// - `AnnounceParams` and `RotateOperator` with any error case of `Engine::announce_params` and
    ///   `Engine::rotate_operator`
    /// - from `V40`, `SetCloseSpreadCeilings` and `AdoptCloseSpreadCeiling` with any error case of
    ///   `Engine::install_close_spread_ceilings` and `Engine::adopt_close_spread_ceiling`;
    ///   `UpsertMarket` of a market the ceiling registry has no entry for, or changing the
    ///   `asset_class` or `quote_currency` of a market with one; `ActivateProtocol` carrying a
    ///   version past `V40` while a listed market has no adopted ceiling (see
    ///   `Engine::check_close_spread_activation`)
    pub(crate) fn admin(&mut self, cmd: &AdminCommand, events: &mut Vec<Event>) -> Result<(), String> {
        match cmd {
            AdminCommand::ScheduleCorporateAction(action) => self.schedule_corporate_action(action, events)?,
            AdminCommand::SetAccessPolicy(policy) => {
                if policy.max_age_ms <= 0 || policy.source == crate::types::AccountId([0; 20]) {
                    return Err("access policy needs a nonzero source and positive freshness bound".into());
                }
                if self.access_policy.is_some() {
                    return Err("the access policy is already recorded".into());
                }
                self.access_policy = Some(policy.clone());
            }
            AdminCommand::SetParams(params) => {
                params.validate_for(self.protocol_version)?;
                if self.protocol_version >= ProtocolVersion::V8 {
                    params.check_closing_spread()?;
                }
                if self.protocol_version >= ProtocolVersion::V11 {
                    params.check_notional_weighting()?;
                }
                if self.protocol_version >= ProtocolVersion::V13 {
                    self.markets.values().try_for_each(|market| params.check_divergence_reachable(&market.spec))?;
                }
                if params.max_resting_orders != DEFAULT_MAX_RESTING_ORDERS
                    && self.protocol_version < ProtocolVersion::V14
                {
                    return Err(format!("a resting-order cap needs protocol {:?}", ProtocolVersion::V14));
                }
                let bounded = self.protocol_version >= ProtocolVersion::V35;
                if bounded {
                    self.check_rate_changes(params)?;
                }
                let rates_before = self.params.as_ref().map(|p| (p.profit_fee_bps, p.closeout_level_bps));
                self.next_rollover = Some(params.rollover.next_after(self.now));
                self.params = Some(params.clone());
                if bounded {
                    self.settle_rate_changes(rates_before, params);
                }
                // From `V16`, a lowered memory reweighs old history at once: without the clamp,
                // the account's next markout still weighs the history it built under the earlier
                // memory, and caps it only then. Raising the memory clamps nothing, since history
                // never exceeds the memory it built under. `SetToxicityNotional` lands only
                // where no history exists to clamp: parameters without weighting predate every
                // notional-weighted markout. Read to select first: a touched account re-encodes
                // its leaf at the next root, so touch only what the clamp changes.
                if self.protocol_version >= ProtocolVersion::V16
                    && let Some(memory) = params.toxicity.notional.map(|weighting| weighting.memory)
                {
                    let over: Vec<_> = self
                        .accounts
                        .iter()
                        .filter(|(_, acct)| acct.toxicity_history > memory)
                        .map(|(account, _)| *account)
                        .collect();
                    for account in over {
                        self.accounts.get_mut(&account).expect("selected above").toxicity_history = memory;
                    }
                }
                self.restress_all(events);
                if self.protocol_version >= ProtocolVersion::V12 {
                    self.advance_markets(None, events);
                }
                if self.protocol_version >= ProtocolVersion::V6 {
                    self.check_all_closeouts(events);
                }
            }
            AdminCommand::UpsertMarket(spec) => {
                spec.validate()?;
                self.check_corporate_listing(spec)?;
                self.check_listing_ceilings(spec)?;
                if spec.schedule.has_exceptions() && self.protocol_version < ProtocolVersion::V4 {
                    return Err(format!("dated closures and special sessions need protocol {:?}", ProtocolVersion::V4));
                }
                if !spec.contracts.is_empty() && self.protocol_version < ProtocolVersion::V21 {
                    return Err(format!("futures contracts need protocol {:?}", ProtocolVersion::V21));
                }
                self.check_quote_currency(spec)?;
                if self.protocol_version >= ProtocolVersion::V35 {
                    self.check_priceable(spec)?;
                }
                if let Some(params) = self.params.as_ref().filter(|_| self.protocol_version >= ProtocolVersion::V13) {
                    params.check_divergence_reachable(spec)?;
                }
                // Before any benchmark has a rate, as in a genesis that leaves every rate to the feed,
                // a listing may name any; after, a new name must be one with a rate.
                let benchmark = &spec.financing.benchmark;
                let named =
                    self.markets.get(&spec.id).is_none_or(|market| market.spec.financing.benchmark != *benchmark);
                if self.protocol_version >= ProtocolVersion::V14
                    && named
                    && (!self.benchmarks.is_empty()
                        || (self.protocol_version >= ProtocolVersion::V18 && !self.benchmark_history.is_empty()))
                    && !self.benchmarks.contains_key(benchmark)
                    && !(self.protocol_version >= ProtocolVersion::V18
                        && self.benchmark_history.contains_key(benchmark))
                {
                    return Err(format!("benchmark {benchmark} has no rate"));
                }
                let currency_changed =
                    self.markets.get(&spec.id).is_some_and(|market| market.spec.quote_currency != spec.quote_currency);
                let futures = match self.markets.get(&spec.id) {
                    Some(market) => market.relisted(spec)?,
                    None => FuturesState::listed(spec, self.now),
                };
                match self.markets.get_mut(&spec.id) {
                    Some(market) => {
                        // An FX mark belongs to the currency it was taken for.
                        if market.spec.quote_currency != spec.quote_currency {
                            market.fx_mark = None;
                        }
                        let old_max_age_ms = market.spec.price_rules.max_age_ms;
                        market.spec = spec.clone();
                        market.futures = futures;
                        // From `V33`, a quote stale under the old limit stays stale: a raised limit
                        // must not bring it back to pricing.
                        if self.protocol_version >= ProtocolVersion::V33 && spec.price_rules.max_age_ms > old_max_age_ms
                        {
                            market.drop_quotes_older_than(self.now, old_max_age_ms);
                        }
                    }
                    None => {
                        self.markets.insert(spec.id, MarketState { futures, ..MarketState::new(spec.clone()) });
                    }
                }
                self.refresh_session(spec.id, events);
                // A new listing for a market others convert at can change what its index converts
                // at, so it is their price update too, before any close-out runs.
                let converts_here = |market: &MarketState| {
                    market.spec.id != spec.id && market.spec.quote_currency.fx_market() == Some(spec.id)
                };
                let reprices = self.protocol_version >= ProtocolVersion::V7
                    && self.params.is_some()
                    && self.markets.values().any(converts_here);
                if self.protocol_version >= ProtocolVersion::V12 {
                    // A new quote currency drops the FX mark, which a market that converts at its own
                    // index takes back from its own index only when it is processed.
                    self.advance_markets((reprices || currency_changed).then_some(spec.id), events);
                } else if reprices {
                    self.reprice(spec.id, events);
                }
                if self.protocol_version >= ProtocolVersion::V6 && self.params.is_some() {
                    self.check_closeouts(spec.id, events);
                }
            }
            AdminCommand::SetBenchmarkRate { benchmark, annual_bps } => {
                if !(-MAX_BENCHMARK_BPS..=MAX_BENCHMARK_BPS).contains(annual_bps) {
                    return Err(format!("benchmark rate {annual_bps}bps out of range"));
                }
                self.benchmarks.insert(benchmark.clone(), *annual_bps);
                if self.protocol_version >= ProtocolVersion::V18 {
                    let date = self.benchmark_date(self.now);
                    self.benchmark_history
                        .entry(benchmark.clone())
                        .or_default()
                        .insert(date, crate::Fixed::from_bps(*annual_bps));
                }
            }
            AdminCommand::SetBenchmarkRates(batch) => self.set_benchmark_rates(batch)?,
            AdminCommand::SetStressFloor { market, level } => {
                let levels = self.params.as_ref().ok_or("protocol params not set")?.stress_levels.len();
                if *level >= levels {
                    return Err(format!("stress level {level} does not exist"));
                }
                match market {
                    None => self.global_stress_floor = *level,
                    Some(id) => self.markets.get_mut(id).ok_or("unknown market")?.stress.floor = *level,
                }
                self.restress_all(events);
            }
            AdminCommand::SetReduceOnly { market: Some(id), reduce_only } => {
                self.markets.get_mut(id).ok_or("unknown market")?.reduce_only = *reduce_only;
            }
            AdminCommand::SetReduceOnly { market: None, reduce_only } => {
                if self.protocol_version < ProtocolVersion::V8 {
                    return Err("reduce-only for every market needs protocol V8".into());
                }
                self.global_reduce_only = *reduce_only;
            }
            AdminCommand::SetClientAuth(auth) => {
                if self.client_auth.is_some() {
                    return Err("client auth is already recorded".into());
                }
                self.client_auth = Some(auth.clone());
            }
            AdminCommand::SetToxicityNotional(weighting) => {
                let params = self.params.as_mut().ok_or("protocol params not set")?;
                if params.toxicity.notional.is_some() {
                    return Err("the parameters in force already weigh markouts by notional".into());
                }
                weighting.validate()?;
                params.toxicity.notional = Some(*weighting);
            }
            AdminCommand::SetOperator(operator) => {
                if self.operator.is_some() {
                    return Err("the operator is already recorded".into());
                }
                self.operator = Some(*operator);
            }
            AdminCommand::ActivateProtocol { version } | AdminCommand::ActivateCustodyFinality { version, .. } => {
                let starts_finality = version.supports_deposit_finality() && !self.deposit_finality_enabled();
                let migration = matches!(cmd, AdminCommand::ActivateCustodyFinality { .. });
                let has_legacy = self.credited_refs.iter().any(|r| legacy_chain_reference(r).is_some());
                if (migration && !starts_finality) || (starts_finality && has_legacy && !migration) {
                    return Err("custody finality activation requires the verified legacy credit ledger".into());
                }
                if *version <= self.protocol_version {
                    return Err(format!("protocol {version:?} is not newer than {:?}", self.protocol_version));
                }
                if *version >= ProtocolVersion::V3 && self.client_auth.is_none() {
                    return Err(format!("protocol {version:?} needs client auth recorded first"));
                }
                if *version >= ProtocolVersion::V8 && self.operator.is_none() {
                    return Err(format!("protocol {version:?} needs the operator recorded first"));
                }
                if *version >= ProtocolVersion::V8
                    && let Some(params) = &self.params
                {
                    params.check_closing_spread()?;
                }
                if *version >= ProtocolVersion::V11
                    && let Some(params) = &self.params
                {
                    params.check_notional_weighting()?;
                }
                self.check_close_spread_activation(*version)?;
                if self.protocol_version < ProtocolVersion::V26 && *version >= ProtocolVersion::V26 {
                    self.check_terminal_receipt_holds()?;
                }
                // Earlier allocations reduced house cash without reserving payout capacity.
                let distribution_cash =
                    if self.protocol_version < ProtocolVersion::V31 && *version >= ProtocolVersion::V31 {
                        let allocated = self.staking.as_ref().map_or(0, |staking| staking.distributed.0);
                        let amount = crate::withdrawal::from_usdc(allocated)
                            .ok_or("inherited distribution reserve exceeds cash representation")?;
                        self.withdrawal_cash
                            .map(|cash| {
                                cash.raw()
                                    .checked_sub(amount.raw())
                                    .map(crate::Fixed::from_raw)
                                    .ok_or("inherited distribution reserve overflows payout capacity")
                            })
                            .transpose()?
                    } else {
                        None
                    };
                if let AdminCommand::ActivateCustodyFinality { legacy_deposits, .. } = cmd {
                    self.lock_legacy_deposits(legacy_deposits)?;
                }
                // Markouts queued before `V11` carry no notional, so they would move no score, and they
                // are in fill order rather than the due order `V11` keeps.
                if self.protocol_version < ProtocolVersion::V11 && *version >= ProtocolVersion::V11 {
                    while self.pop_markout().is_some() {}
                }
                // Quotes stored under earlier rules may come from outside the session open now.
                if self.protocol_version < ProtocolVersion::V21 && *version >= ProtocolVersion::V21 {
                    let now = self.now;
                    let ids: Vec<MarketId> = self.markets.keys().copied().collect();
                    for market_id in ids {
                        self.markets.get_mut(&market_id).expect("listed").retain_session_quotes(now);
                    }
                }
                if self.protocol_version < ProtocolVersion::V13 && *version >= ProtocolVersion::V13 {
                    let widest = self.markets.values().map(|market| market.spec.price_rules.max_divergence_bps).max();
                    if let Some(params) = self.params.as_mut() {
                        params.share_divergence_triggers(widest.unwrap_or(0));
                    }
                }
                // Payout capacity is the conserved cash whose principal is final; locked principal joins
                // it when its receipt finalises.
                if self.protocol_version < ProtocolVersion::V22 && *version >= ProtocolVersion::V22 {
                    let unlocked = |cash: crate::Fixed, locked: crate::Fixed| cash - locked;
                    self.withdrawal_cash = Some(
                        unlocked(self.house_cash, self.unfinalised_house_deposits)
                            + self
                                .accounts
                                .values()
                                .map(|acct| unlocked(acct.balance, acct.unfinalised_deposits))
                                .sum::<crate::Fixed>(),
                    );
                }
                if self.protocol_version < ProtocolVersion::V27 && *version >= ProtocolVersion::V27 {
                    self.tree = None;
                }
                if self.protocol_version < ProtocolVersion::V35 && *version >= ProtocolVersion::V35 {
                    self.record_launch_rates();
                }
                // Queue and pending leaves take their exit encodings, and the exit terms leaf joins.
                if self.protocol_version < ProtocolVersion::V38 && *version >= ProtocolVersion::V38 {
                    self.tree = None;
                }
                if let Some(cash) = distribution_cash {
                    self.withdrawal_cash = Some(cash);
                }
                // The exit refunds pending deposits by id, which two live receipts at one id would
                // alias: every duplicate above the finalised frontier resolves to its earliest live
                // receipt, before the version this failure would refuse takes effect.
                if self.protocol_version < ProtocolVersion::V38 && *version >= ProtocolVersion::V38 {
                    self.reverse_duplicate_live_receipts(events)?;
                }
                self.protocol_version = *version;
                if *version >= ProtocolVersion::V26 {
                    self.hot_indexes = None;
                    self.ensure_hot_indexes();
                }
            }
            AdminCommand::InitEmissions(schedule) => self.init_emissions(schedule)?,
            AdminCommand::InitStaking(config) => self.init_staking(config)?,
            AdminCommand::AnnounceParams(notice) => self.announce_params(notice)?,
            AdminCommand::RotateOperator { address } => self.rotate_operator(*address)?,
            AdminCommand::SetCloseSpreadCeilings { ceilings } => self.install_close_spread_ceilings(ceilings)?,
            AdminCommand::AdoptCloseSpreadCeiling { market, max_close_spread_bps, effective_at } => {
                self.adopt_close_spread_ceiling(*market, *max_close_spread_bps, *effective_at)?
            }
        }
        Ok(())
    }

    /// Applies an admin command the node sequences unsigned: client auth, the operator, toxicity
    /// notional weighting, and protocol activation at any time, and genesis, which credits its house
    /// capital last.
    /// Error cases:
    /// - a corporate instruction, which only the signed operator channel accepts
    /// - from `V8`, an operator command once any deposit or house capital is credited
    /// - the emission schedule, which only the operator's signature initialises
    /// - a parameter notice or an operator rotation, which only the operator's signature records
    /// - any error case of `admin`
    pub(crate) fn unsigned_admin(&mut self, cmd: &AdminCommand, events: &mut Vec<Event>) -> Result<(), String> {
        if matches!(cmd, AdminCommand::ScheduleCorporateAction(_)) {
            return Err("corporate instructions require the signed operator channel".into());
        }
        if self.protocol_version >= ProtocolVersion::V8
            && cmd.is_operator_command()
            && (!self.credited_refs.is_empty() || !self.deposits.is_empty())
        {
            return Err("from protocol V8 the operator signs this command once funds are credited".into());
        }
        if matches!(cmd, AdminCommand::InitEmissions(_)) {
            return Err("the operator signs the emission schedule".into());
        }
        if matches!(cmd, AdminCommand::AnnounceParams(_) | AdminCommand::RotateOperator { .. }) {
            return Err("the operator signs a parameter notice or a rotation".into());
        }
        self.admin(cmd, events)
    }

    /// Applies an admin command the operator signed. A command whose signature and nonce pass uses
    /// the nonce even if it is then refused, so it cannot be replayed from the log.
    /// Error cases:
    /// - before `V8`; the nonce is not used
    /// - a signature that is not the recorded operator's over this command, a nonce other than the
    ///   operator's next, or an expiry passed or too far ahead (see `Engine::authorise_admin`)
    /// - a command only the node sequences: `SetClientAuth`, `SetOperator`, `SetToxicityNotional`, or
    ///   `ActivateProtocol`
    /// - any error case of `admin`
    pub(crate) fn signed_admin(&mut self, req: &AdminRequest, events: &mut Vec<Event>) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V8 {
            return Err("signed admin commands need protocol V8".into());
        }
        self.authorise_admin(&req.command, &req.auth).map_err(|reject| auth_failure(&reject))?;
        if !req.command.is_operator_command() {
            return Err("the operator may not send this command".into());
        }
        self.admin(&req.command, events)
    }

    /// Checks a listing's quote currency against the protocol version and the markets already listed.
    /// Before `V7` every market is quoted in USD, whatever its symbol.
    /// Error cases:
    /// - a quote currency other than USD before `V7`
    /// - from `V7`, a listing that fails `MarketSpec::check_fx_market`, or that makes a market which
    ///   converts at it fail it
    /// - a change of quote currency while an account holds a position in the market
    fn check_quote_currency(&self, spec: &MarketSpec) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V7 {
            if spec.quote_currency.is_usd() {
                return Ok(());
            }
            return Err(format!("a quote currency other than USD needs protocol {:?}", ProtocolVersion::V7));
        }
        let symbol = |id: MarketId| {
            if id == spec.id {
                Some(spec.symbol.as_str())
            } else {
                self.markets.get(&id).map(|market| market.spec.symbol.as_str())
            }
        };
        spec.check_fx_market(symbol)?;
        // A market others convert at must stay their pair.
        let dependents = self.markets.values().map(|market| &market.spec);
        for dependent in dependents.filter(|d| d.id != spec.id && d.quote_currency.fx_market() == Some(spec.id)) {
            dependent.check_fx_market(symbol).map_err(|e| format!("market {} converts here: {e}", dependent.id))?;
        }
        let changed =
            self.markets.get(&spec.id).is_some_and(|market| market.spec.quote_currency != spec.quote_currency);
        if changed && self.accounts.values().any(|acct| acct.positions.contains_key(&spec.id)) {
            return Err(format!("market {} has open positions, so its quote currency cannot change", spec.id));
        }
        Ok(())
    }
}

/// The reason an admin command fails its signature, nonce, or expiry check.
fn auth_failure(reject: &Reject) -> String {
    match reject {
        Reject::InvalidSignature => "invalid signature".into(),
        Reject::InvalidNonce { expected } => format!("invalid nonce: expected {expected}"),
        Reject::Expired { expires_at } => format!("expired at {expires_at}"),
        Reject::ExpiryTooFar { latest } => format!("expiry after the latest allowed, {latest}"),
        // `authorise_admin` refuses only for the reasons above.
        other => serde_json::to_string(other).expect("rejections serialise"),
    }
}
