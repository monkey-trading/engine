//! A fill's markout from booking to its score: before `V54` as `markouts` queues it, and from `V54`
//! with its index, its quantity and the round trips it is part of.
//!
//! A fill's markout is there for matching while it is within the horizon: its due time is after the
//! sequenced time. The part of a fill that closes quantity matches the account's markouts in the
//! market newest-first, up to the quantity it closes, taking from each the quantity it still has
//! open; the part that opens quantity takes, the same way, from the quantity the account's opposite
//! fills just closed, so a close and the reopening after it are a round trip as an open and the close
//! after it are. Where a markout's fill was at the new fill's index, the quantity taken is a round
//! trip, and counts as matched on both. The quantity open on a markout is the part of its fill that
//! opened the position, so the open markouts are always the newest part of the position; a close-out
//! closes quantity too, but matches none and is not marked. A split ends matching for the fills
//! before it.

use std::collections::BTreeMap;

use crate::Fixed;
use crate::engine::Engine;
use crate::execution::Exec;
use crate::fixed::Rounding;
use crate::markouts::{Markout, markout_bps};
use crate::protocol::ProtocolVersion;
use crate::toxicity::HORIZON_MS;
use crate::toxicity::legs::Leg;
use crate::types::MarketId;

/// What booking a fill tells the score about it.
pub(crate) struct FillMark {
    /// The account's position in the market before the fill.
    pub(crate) held: Fixed,
    pub(crate) qty: Fixed,
    /// The index the fill priced from.
    pub(crate) index: Fixed,
    pub(crate) price: Fixed,
    /// The FX index the fill converts to USD at; `None` for a USD market.
    pub(crate) fx: Option<Fixed>,
}

/// What a market's markouts are scored against in one pass.
#[derive(Clone, Copy)]
enum Unit {
    /// The market's listing names no floor, which none does from `V54`: its markouts score nothing.
    Unscored,
    /// No tradeable index yet: its due markouts wait.
    Waiting,
    /// The tradeable index, and the markout spread in bps.
    Ready(Fixed, Fixed),
}

impl Engine {
    /// Queues a markout for a booked fill. From `V54`, a fill first matches round trips with the
    /// part that closes and then the part that opens (see the module docs); its markout is measured
    /// from its index, carries its quantity, the quantity it matched and the quantities it opened and
    /// closed that are left to match, and falls due `HORIZON_MS` later. A close-out is not marked,
    /// though from `V54` the quantity it closes is no longer open.
    pub(crate) fn mark_fill(&mut self, exec: &Exec, mark: FillMark) {
        let (account, market, side) = (exec.account, exec.market, exec.side);
        let quote = self.markets[&market].spec.quote_currency;
        let notional = quote.to_usd(mark.qty.mul(mark.price), mark.fx, Rounding::Down);
        if self.protocol_version < ProtocolVersion::V54 {
            if !exec.liquidation {
                let horizon_ms = self.params.as_ref().expect("callers check params are set").toxicity.horizon_ms;
                let (due, price, zero) = (self.now + horizon_ms, mark.price, Fixed::ZERO);
                let (qty, matched, open, closed) = (zero, zero, zero, zero);
                let markout = Markout { due, account, market, side, price, notional, qty, matched, open, closed };
                self.record_markout(markout);
            }
            return;
        }
        let closes = !mark.held.is_zero() && mark.held.signum() != side.sign();
        let closing = if closes { mark.qty.min(mark.held.abs()) } else { Fixed::ZERO };
        let at = (!exec.liquidation).then_some(mark.index);
        let closed_matched = self.match_round_trips((account, market, side), closing, at, Leg::Open);
        if exec.liquidation {
            return;
        }
        // A close-out only reduces, so it opens nothing.
        let opening = mark.qty - closing;
        let opened_matched = self.match_round_trips((account, market, side), opening, at, Leg::Closed);
        let (due, price, qty) = (self.now + HORIZON_MS, mark.index, mark.qty);
        let (matched, open, closed) =
            (closed_matched + opened_matched, opening - opened_matched, closing - closed_matched);
        let newest = self.markouts.back().is_none_or(|last| last.due <= due);
        self.record_markout(Markout { due, account, market, side, price, notional, qty, matched, open, closed });
        self.index_markout(newest);
    }

    /// From `V54`, scores each markout that has fallen due: the share of its fill no round trip
    /// matched, against its market's tradeable index now, in standard errors of the market's markout
    /// spread (see `toxicity`). A markout with no tradeable index waits for one, and one still
    /// waiting a horizon after it fell due, or one fully matched, is dropped unscored.
    pub(crate) fn score_toxicity(&mut self) {
        let now = self.now;
        if self.markouts.front().is_none_or(|markout| markout.due > now) {
            return;
        }
        let mut waiting = Vec::new();
        // Each market's unit, worked out once for the pass.
        let mut units: BTreeMap<MarketId, Unit> = BTreeMap::new();
        while self.markouts.front().is_some_and(|markout| markout.due <= now) {
            let markout = self.pop_markout().expect("front exists");
            self.retire_markout(&markout);
            let unmatched = markout.qty - markout.matched;
            if now - markout.due > HORIZON_MS || !unmatched.is_positive() {
                continue;
            }
            let unit = *units.entry(markout.market).or_insert_with(|| self.markout_unit(markout.market));
            let (index, spread) = match unit {
                Unit::Unscored => continue,
                Unit::Waiting => {
                    waiting.push(markout);
                    continue;
                }
                Unit::Ready(index, spread) => (index, spread),
            };
            let errors = markout_bps(&markout, index).div_rounded(spread, Rounding::Up);
            let notional = markout.notional.mul_div(unmatched, markout.qty, Rounding::Down);
            let share = unmatched.div_rounded(markout.qty, Rounding::Down);
            let score = self.toxicity_scores.entry(markout.account).or_default();
            *score = score.decayed_to(now);
            score.fold(notional, share, errors);
        }
        // Back at the front in due order, to try again at the next command.
        waiting.into_iter().rev().for_each(|markout| self.requeue_markout(markout));
    }

    /// What `market`'s markouts score against now.
    fn markout_unit(&self, market: MarketId) -> Unit {
        let Some(spread) = self.markout_spread_bps(market) else { return Unit::Unscored };
        match self.tradeable_index(market) {
            Some((index, _)) => Unit::Ready(index, spread),
            None => Unit::Waiting,
        }
    }
}
