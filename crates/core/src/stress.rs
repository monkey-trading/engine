//! Stress levels. Each market's automatic level rises at once when any reading meets a level's
//! trigger and steps down only after the cooldown; its effective level is the highest of that and
//! the operator's floors. From `V17`, a market's move window survives a halt: prices observed
//! within the window before the market closed still count after it reopens. From `V46` an operator's
//! floor is at most `MAX_OPERATOR_STRESS_FLOOR` and lapses to the automatic level
//! `STRESS_FLOOR_LAPSE_MS` of sequenced time after it is set, so only the automatic triggers reach
//! the higher levels and hold them.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::command::Command;
use crate::engine::{Engine, MarketState};
use crate::event::Event;
use crate::fixed::Rounding;
use crate::params::{StressLevel, StressTrigger};
use crate::protocol::ProtocolVersion;
use crate::types::{MarketId, Millis};

/// From `V13`, the move window keeps one low and high per tenth of `stress_move_window_ms`, so while
/// the window is unchanged it holds at most a dozen buckets per market, however often prices arrive.
const MOVE_BUCKETS: i64 = 10;
/// From `V46`, the highest stress floor the operator can set, in a market or in all of them.
pub const MAX_OPERATOR_STRESS_FLOOR: usize = 1;
/// From `V46`, how long an operator's stress floor lasts unless it is set again: 72 hours.
pub const STRESS_FLOOR_LAPSE_MS: Millis = 72 * 60 * 60 * 1000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StressState {
    /// Effective level: max(auto, floor, global floor).
    pub level: usize,
    pub auto: usize,
    pub auto_since: Millis,
    pub floor: usize,
    /// From `V46`, when a floor above level 0 was set, which starts its time to lapse. Left out of
    /// the encoding while empty, so earlier roots do not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_set_at: Option<Millis>,
    /// Before `V13`, the time and index mid the current move window started at.
    pub move_ref: Option<(Millis, Fixed)>,
    /// From `V13`, the index mids of the trailing move window in time order, as the range of each
    /// bucket. Left out of the encoding while empty, so earlier roots do not change.
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    pub move_window: VecDeque<MoveBucket>,
}

/// The lowest and highest index mid a market had from `from` to `to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveBucket {
    pub from: Millis,
    pub to: Millis,
    pub low: Fixed,
    pub high: Fixed,
}

/// A market's stress readings at one moment.
struct Readings {
    /// Venue divergence in bps of the index.
    divergence_bps: i64,
    /// Venue divergence as a share of the market's divergence bound, in bps.
    divergence_share_bps: i64,
    /// Largest move in bps within the move window.
    move_bps: i64,
    /// House net exposure in the market as a share of its cap, in bps.
    utilisation_bps: i64,
}

impl Engine {
    /// Before `V13`: recomputes a market's automatic stress level from the latest index, then its
    /// effective level. A raised automatic level steps down only after the cooldown.
    pub(crate) fn update_stress(
        &mut self,
        market_id: MarketId,
        mid: Fixed,
        divergence_bps: i64,
        events: &mut Vec<Event>,
    ) {
        let cap = self.market_cap(&self.markets[&market_id].spec, self.house_nav());
        let now = self.now;
        let global_floor = self.global_stress_floor;
        let params = self.params.as_ref().expect("callers check params are set");
        let market = self.markets.get_mut(&market_id).expect("callers pass a listed market");

        let move_bps = match market.stress.move_ref {
            Some((t, p)) if now - t < params.stress_move_window_ms => (mid - p).abs().bps_of(p),
            _ => {
                market.stress.move_ref = Some((now, mid));
                0
            }
        };
        let readings = Readings {
            divergence_bps,
            divergence_share_bps: divergence_share_bps(market, divergence_bps),
            move_bps,
            utilisation_bps: utilisation_bps(market, mid, market.fx_mark, cap),
        };
        let target = auto_stress_target(&params.stress_levels, &readings);
        step_auto(&mut market.stress, target, now, params.stress_cooldown_ms);
        let max_level = params.stress_levels.len() - 1;
        set_stress_level(market_id, market, global_floor, max_level, events);
    }

    /// From `V13`: re-evaluates a market's automatic stress level from its index now, its house
    /// exposure at that index in USD at the live FX index, and its cap at house NAV `nav`, then its
    /// effective level. The index mid joins the move window. A raised automatic level steps down
    /// only after the cooldown. A market that is closed, or has no index or no live FX index, keeps
    /// its level, since nothing fills there.
    pub(crate) fn refresh_stress(&mut self, market_id: MarketId, nav: Fixed, events: &mut Vec<Event>) {
        let now = self.now;
        let global_floor = self.global_stress_floor;
        let market = self.markets.get(&market_id).expect("callers pass a listed market");
        let cap = self.market_cap(&market.spec, nav);
        if !market.open {
            return;
        }
        let Ok(index) = market.index(now) else { return };
        // A fill converts at the live FX index, so the reading does too.
        let Ok(fx) = self.live_fx(market_id) else { return };
        let Some(params) = self.params.as_ref() else { return };
        // Only a market with a reading is handed out for change, so the state root rehashes no other.
        let market = self.markets.get_mut(&market_id).expect("listed");

        // The window takes over the move reference the rules before `V13` keep.
        if let Some((from, mid)) = market.stress.move_ref.take() {
            market.stress.move_window.push_front(MoveBucket { from, to: from, low: mid, high: mid });
        }
        let move_bps = record_move(&mut market.stress.move_window, now, index.mid, params.stress_move_window_ms);
        let readings = Readings {
            divergence_bps: index.divergence_bps,
            divergence_share_bps: divergence_share_bps(market, index.divergence_bps),
            move_bps,
            utilisation_bps: utilisation_bps(market, index.mid, fx, cap),
        };
        let target = auto_stress_target(&params.stress_levels, &readings);
        step_auto(&mut market.stress, target, now, params.stress_cooldown_ms);
        let max_level = params.stress_levels.len() - 1;
        set_stress_level(market_id, market, global_floor, max_level, events);
    }

    /// From `V13`: re-evaluates every market's stress level after `cmd`, at one house NAV, since any
    /// command can move a stress input: time, prices, exposure, house NAV, or the rules. An admin
    /// command's outcome stays its last event.
    pub(crate) fn refresh_all_stress(&mut self, cmd: &Command, events: &mut Vec<Event>) {
        if self.protocol_version < ProtocolVersion::V13 {
            return;
        }
        let admin = matches!(cmd, Command::Admin(_) | Command::SignedAdmin(_));
        let outcome = if admin { events.pop() } else { None };
        let nav = self.house_nav();
        let ids: Vec<MarketId> = self.markets.keys().copied().collect();
        for market_id in ids {
            self.refresh_stress(market_id, nav, events);
        }
        events.extend(outcome);
    }

    /// Sets the operator's stress floor in `market`, or in every market, starting from `V46` its time
    /// to lapse.
    /// Error cases:
    /// - from `V46`, a level above `MAX_OPERATOR_STRESS_FLOOR`
    /// - an unknown market
    pub(crate) fn set_stress_floor(&mut self, market: Option<MarketId>, level: usize) -> Result<(), String> {
        let bounded = self.protocol_version >= ProtocolVersion::V46;
        if bounded && level > MAX_OPERATOR_STRESS_FLOOR {
            return Err(format!("the operator's stress floor is at most level {MAX_OPERATOR_STRESS_FLOOR}"));
        }
        let set_at = (bounded && level > 0).then_some(self.now);
        match market {
            None => {
                self.global_stress_floor = level;
                self.operator_bounds.global_floor_set_at = set_at;
            }
            Some(id) => {
                let stress = &mut self.markets.get_mut(&id).ok_or("unknown market")?.stress;
                stress.floor = level;
                stress.floor_set_at = set_at;
            }
        }
        Ok(())
    }

    /// When `V46` activates: a floor above `MAX_OPERATOR_STRESS_FLOOR` drops to it, and every floor
    /// in force starts its time to lapse from now, since no earlier rule recorded when it was set.
    pub(crate) fn bound_stress_floors(&mut self, events: &mut Vec<Event>) {
        let now = self.now;
        let held: Vec<MarketId> =
            self.markets.iter().filter(|(_, market)| market.stress.floor > 0).map(|(id, _)| *id).collect();
        for id in held {
            let stress = &mut self.markets.get_mut(&id).expect("listed").stress;
            stress.floor = stress.floor.min(MAX_OPERATOR_STRESS_FLOOR);
            stress.floor_set_at = Some(now);
        }
        if self.global_stress_floor > 0 {
            self.global_stress_floor = self.global_stress_floor.min(MAX_OPERATOR_STRESS_FLOOR);
            self.operator_bounds.global_floor_set_at = Some(now);
        }
        self.restress_all(events);
    }

    /// From `V46`: drops each floor whose `STRESS_FLOOR_LAPSE_MS` has run by the engine's time, leaving
    /// the automatic level. The corporate timeline runs it at each boundary it walks and at the
    /// command's own time, before the command acts, so a boundary before the lapse is priced with the
    /// floor. The bound on the work is the listed markets, which `refresh_all_stress` walks for every
    /// command anyway.
    pub(crate) fn lapse_stress_floors(&mut self, events: &mut Vec<Event>) {
        if self.protocol_version < ProtocolVersion::V46 {
            return;
        }
        let now = self.now;
        let due = |set_at: Option<Millis>| set_at.is_some_and(|at| now - at >= STRESS_FLOOR_LAPSE_MS);
        let lapsed: Vec<MarketId> =
            self.markets.iter().filter(|(_, market)| due(market.stress.floor_set_at)).map(|(id, _)| *id).collect();
        let global = due(self.operator_bounds.global_floor_set_at);
        if lapsed.is_empty() && !global {
            return;
        }
        for id in lapsed {
            let stress = &mut self.markets.get_mut(&id).expect("listed").stress;
            stress.floor = 0;
            stress.floor_set_at = None;
        }
        if global {
            self.global_stress_floor = 0;
            self.operator_bounds.global_floor_set_at = None;
        }
        self.restress_all(events);
    }

    /// Recomputes every market's effective stress level after a floor or parameter change.
    pub(crate) fn restress_all(&mut self, events: &mut Vec<Event>) {
        let Some(params) = &self.params else { return };
        let max_level = params.stress_levels.len() - 1;
        let global_floor = self.global_stress_floor;
        for (market_id, market) in self.markets.iter_mut() {
            set_stress_level(*market_id, market, global_floor, max_level, events);
        }
    }
}

/// The highest stress level whose trigger any current reading meets, or 0.
fn auto_stress_target(levels: &[StressLevel], readings: &Readings) -> usize {
    levels
        .iter()
        .enumerate()
        .skip(1)
        .rev()
        .find(|(_, level)| level.trigger.as_ref().is_some_and(|t| is_met(t, readings)))
        .map_or(0, |(i, _)| i)
}

/// Whether any of the trigger's thresholds is met.
fn is_met(trigger: &StressTrigger, readings: &Readings) -> bool {
    trigger.divergence_bps.is_some_and(|x| readings.divergence_bps >= x)
        || trigger.divergence_share_bps.is_some_and(|x| readings.divergence_share_bps >= x)
        || trigger.move_bps.is_some_and(|x| readings.move_bps >= x)
        || trigger.utilisation_bps.is_some_and(|x| readings.utilisation_bps >= x)
}

/// `divergence_bps` as a share of the market's divergence bound. A market with a zero bound
/// prices only while its venues agree, so its share is zero.
fn divergence_share_bps(market: &MarketState, divergence_bps: i64) -> i64 {
    let bound = market.spec.price_rules.max_divergence_bps;
    if bound > 0 { divergence_bps * 10_000 / bound } else { 0 }
}

/// The house's net exposure in the market at `mid`, in USD at `fx` and rounded up, as a share of
/// `cap`; full when the cap is not positive.
fn utilisation_bps(market: &MarketState, mid: Fixed, fx: Option<Fixed>, cap: Fixed) -> i64 {
    let exposure = market.spec.quote_currency.to_usd(market.client_qty.abs().mul(mid), fx, Rounding::Up);
    if cap.is_positive() { exposure.bps_of(cap) } else { i64::MAX }
}

/// Moves the automatic level to `target`: up at once, down only after the cooldown.
fn step_auto(stress: &mut StressState, target: usize, now: Millis, cooldown_ms: i64) {
    if target > stress.auto || (target < stress.auto && now - stress.auto_since >= cooldown_ms) {
        stress.auto = target;
        stress.auto_since = now;
    }
}

/// Adds `mid` at `now` to the move window, drops each bucket whose newest mid is more than
/// `window_ms` old, and returns the largest move in bps to `mid` from the window's lowest or
/// highest mid. A mid counts as long as its bucket does, so the window reaches back at least
/// `window_ms` and less than one bucket, as wide as when it was made, further.
fn record_move(window: &mut VecDeque<MoveBucket>, now: Millis, mid: Fixed, window_ms: i64) -> i64 {
    while window.front().is_some_and(|bucket| now - bucket.to > window_ms) {
        window.pop_front();
    }
    let width = (window_ms / MOVE_BUCKETS).max(1);
    match window.back_mut() {
        Some(bucket) if now - bucket.from < width => {
            bucket.to = now;
            bucket.low = bucket.low.min(mid);
            bucket.high = bucket.high.max(mid);
        }
        _ => window.push_back(MoveBucket { from: now, to: now, low: mid, high: mid }),
    }
    let low = window.iter().map(|bucket| bucket.low).min().expect("the window holds mid");
    let high = window.iter().map(|bucket| bucket.high).max().expect("the window holds mid");
    (mid - low).bps_of(low).max((high - mid).bps_of(high))
}

/// Sets a market's effective stress level to the highest of its automatic level and both floors,
/// capped at the top level, emitting a change.
fn set_stress_level(
    market_id: MarketId,
    market: &mut MarketState,
    global_floor: usize,
    max_level: usize,
    events: &mut Vec<Event>,
) {
    let stress = &mut market.stress;
    let level = stress.auto.max(stress.floor).max(global_floor).min(max_level);
    if level != stress.level {
        stress.level = level;
        events.push(Event::StressChanged { market: market_id, level });
    }
}
