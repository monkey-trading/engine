//! Operator commands.
//!
//! Every admin action is a sequenced command, so the public log shows each parameter change, each
//! listing, and each stress override. From `V8` the operator signs its commands (listings,
//! parameters, benchmark rates, stress floors, and reduce-only) once anything is credited, and the
//! log carries each signature; from `V19` the emission schedule applies only signed, credited or
//! not. Client auth, the operator, and protocol activation stay commands only the node sequences.
//! The operator can raise a market's stress level but never push it below what the automatic
//! triggers set, and from `V8` neither a reduce-only mode nor a spread stops a trade that reduces
//! or closes a position in an open market with a price. From `V56`, while rotation is on, only a
//! passed vote applies the governed commands (`governance`).

use crate::command::{AdminCommand, AdminRequest};
use crate::deposits::legacy_chain_reference;
use crate::engine::{Engine, MarketState};
use crate::event::{Event, Reject};
use crate::futures::FuturesState;
use crate::params::{DEFAULT_MAX_RESTING_ORDERS, MAX_BENCHMARK_BPS};
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

mod quote_currency;

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
    /// - `SetBenchmarkRate` outside ±10000 bps a year, or from `V46` a rate the cap refuses
    /// - `SetStressFloor` before parameters are set, at a level that does not exist, for an unknown
    ///   market, or from `V46` above level 1
    /// - `SetReduceOnly` for an unknown market, or for every market before `V8`
    /// - `SetClientAuth` once client auth is recorded
    /// - `SetOperator` once the operator is recorded
    /// - `ActivateProtocol` for a version no newer than the one in force, for `V3` or later before
    ///   client auth is recorded, or for `V8` or later before the operator is recorded or while the
    ///   parameters as it leaves them fail `Engine::check_activation_closing_spread`, or for `V11` or
    ///   later while they fail `ProtocolParams::check_notional_weighting`
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
    /// - from `V49`, `SetParams` that changes the stress parameters without a notice whose time has
    ///   come, and from `V55` the profit fee below its launch value or the profit fee curve outside
    ///   its bound or without such a notice, and from `V59` the volume tier table likewise (see
    ///   `Engine::check_rate_changes`); `UpsertMarket` that changes a listed market's markup, or from
    ///   `V52` its bands, without one, or from `V59` bands outside a tier's bound (`Engine::check_listing_change`); a markup or
    ///   listing notice with any error case of `Engine::announce_markup` or `Engine::announce_listing`
    /// - from `V40`, `SetCloseSpreadCeilings` and `AdoptCloseSpreadCeiling` with any error case of
    ///   `Engine::install_close_spread_ceilings` and `Engine::adopt_close_spread_ceiling`;
    ///   `UpsertMarket` of a market the ceiling registry has no entry for, or changing the
    ///   `asset_class` or `quote_currency` of a market with one; `ActivateProtocol` carrying a
    ///   version past `V40` while a listed market has no adopted ceiling, or to `V52` over a low one (see
    ///   `Engine::check_close_spread_activation`), or to `V59` over a listing a volume tier's bound refuses
    ///   (`Engine::check_volume_tier_activation`); from `V54`, a markout floor `Engine::check_markout_floor` refuses
    /// - from `V56`, `SetGovernance` with any error case of `Engine::set_governance`; `SetParams` that
    ///   removes a stress level a restriction holds a floor at (see `Engine::check_restricted_levels`)
    ///
    /// A `voted` command is a passed vote's, whose seven days' notice replaces the parameter, markup
    /// and ceiling notices. From `V56` new parameters and listings set the baseline, and the values in
    /// force stay the stricter of it and the restrictions (`governance`).
    pub(crate) fn admin(&mut self, cmd: &AdminCommand, voted: bool, events: &mut Vec<Event>) -> Result<(), String> {
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
                    self.check_rate_changes(params, voted)?;
                }
                self.check_restricted_levels(params)?;
                let before = self.baseline_params().filter(|_| bounded);
                self.next_rollover = Some(params.rollover.next_after(self.now));
                self.params = Some(params.clone());
                self.rebase(None);
                if bounded {
                    self.settle_rate_changes(before.as_ref(), params);
                }
                self.clamp_toxicity_history(params);
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
                self.check_markout_floor(spec)?;
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
                self.check_held_benchmark(spec)?;
                let change = self.check_listing_change(spec, voted)?;
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
                self.end_unpriced_interval(spec.id);
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
                self.end_unpriced_interval(spec.id);
                self.rebase(Some(spec.id));
                self.settle_listing_change(spec, change);
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
                self.check_undated_benchmark_rate(benchmark, *annual_bps)?;
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
                self.set_stress_floor(*market, *level)?;
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
                self.check_activation_closing_spread(*version)?;
                if *version >= ProtocolVersion::V11
                    && let Some(params) = &self.params
                {
                    params.check_notional_weighting()?;
                }
                self.check_close_spread_activation(*version)?;
                self.check_volume_tier_activation(*version)?;
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
                // Open period results rehash under the V43 gross-loss encoding, so the cached
                // tree's leaves cannot be trusted across the activation.
                if self.protocol_version < ProtocolVersion::V43 && *version >= ProtocolVersion::V43 {
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
                // The lifetime count of introduced session keys becomes a refilling budget.
                if self.protocol_version < ProtocolVersion::V42 && *version >= ProtocolVersion::V42 {
                    self.start_introduction_budgets();
                }
                if self.protocol_version < ProtocolVersion::V46 && *version >= ProtocolVersion::V46 {
                    self.bound_stress_floors(events);
                }
                self.start_listing_bounds(*version);
                self.carry_kept_notices(*version);
                self.start_toxicity_score(*version);
                self.floor_bound_listings(*version);
                self.start_hold_clocks(*version);
                self.start_volume_tiers(*version);
                let counts_office = self.protocol_version < ProtocolVersion::V50 && *version >= ProtocolVersion::V50;
                self.protocol_version = *version;
                // Rules before `V47` record no candidate, so one an election already seated is seated before
                // any time counts.
                if counts_office {
                    self.settle_operator_candidate();
                    self.start_time_in_office();
                }
                if *version >= ProtocolVersion::V26 {
                    self.hot_indexes = None;
                    self.ensure_hot_indexes();
                }
            }
            AdminCommand::InitEmissions(schedule) => self.init_emissions(schedule)?,
            AdminCommand::InitStaking(config) => self.init_staking(config)?,
            AdminCommand::AnnounceParams(notice) => self.announce_params(notice)?,
            AdminCommand::AnnounceMarkup { market, markup_bps, effective_at } => {
                self.announce_markup(*market, *markup_bps, *effective_at)?
            }
            AdminCommand::AnnounceListing { market, notice } => self.announce_listing(*market, notice)?,
            AdminCommand::RotateOperator { address } => self.rotate_operator(*address)?,
            AdminCommand::SetCloseSpreadCeilings { ceilings } => self.install_close_spread_ceilings(ceilings)?,
            AdminCommand::AdoptCloseSpreadCeiling { market, max_close_spread_bps, effective_at, listing } => {
                self.adopt_close_spread_ceiling(*market, *max_close_spread_bps, *effective_at, listing.as_ref(), voted)?
            }
            AdminCommand::SetGovernance(config) => self.set_governance(config)?,
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
    /// - a parameter, markup or listing notice or an operator rotation, which only the operator's signature
    ///   records
    /// - from `V56`, while rotation is on, a command only a vote applies (see
    ///   `Engine::refused_outside_vote`)
    /// - any error case of `admin`
    pub(crate) fn unsigned_admin(&mut self, cmd: &AdminCommand, events: &mut Vec<Event>) -> Result<(), String> {
        if self.refused_outside_vote(cmd) {
            return Err("while rotation is on, only a passed vote applies this command".into());
        }
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
        if matches!(
            cmd,
            AdminCommand::AnnounceParams(_)
                | AdminCommand::AnnounceMarkup { .. }
                | AdminCommand::AnnounceListing { .. }
                | AdminCommand::RotateOperator { .. }
        ) {
            return Err("the operator signs a parameter notice or a rotation".into());
        }
        self.admin(cmd, false, events)
    }

    /// Applies an admin command the operator signed. A command whose signature and nonce pass uses
    /// the nonce even if it is then refused, so it cannot be replayed from the log.
    /// Error cases:
    /// - before `V8`; the nonce is not used
    /// - a signature that is not the recorded operator's over this command, a nonce other than the
    ///   operator's next, or an expiry passed or too far ahead (see `Engine::authorise_admin`)
    /// - a command only the node sequences: `SetClientAuth`, `SetOperator`, `SetToxicityNotional`, or
    ///   `ActivateProtocol`
    /// - from `V56`, while rotation is on, a command only a vote applies (see
    ///   `Engine::refused_outside_vote`)
    /// - any error case of `admin`
    pub(crate) fn signed_admin(&mut self, req: &AdminRequest, events: &mut Vec<Event>) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V8 {
            return Err("signed admin commands need protocol V8".into());
        }
        self.authorise_admin(&req.command, &req.auth).map_err(|reject| auth_failure(&reject))?;
        if !req.command.is_operator_command() {
            return Err("the operator may not send this command".into());
        }
        if self.refused_outside_vote(&req.command) {
            return Err("while rotation is on, only a passed vote changes this".into());
        }
        self.admin(&req.command, false, events)
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
