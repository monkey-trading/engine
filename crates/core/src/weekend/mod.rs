//! Weekend margins, from `V59`.
//!
//! A market may carry a weekend margin (`MarketSpec::weekend_margin_bps`): a rate each of its margin
//! bands charges at least over its weekend gap, any closure of at least `WEEKEND_GAP_MS` by its
//! calendar, so a position held across the gap at the market's own leverage cannot take the gap's
//! jump at the house's expense. The market keeps its next or current gap (`MarketState::weekend`),
//! which its listing sets and each gap's reopening moves on to the next (`schedule`).
//!
//! From `WEEKEND_LEAD_MS` before a gap starts until the market reopens, the account's margin
//! requirement counts each position in the market at its weekend margin wherever equity would leave
//! or risk grow: a fill that adds risk in any market (`Reject::WeekendMargin`) and a withdrawal. When
//! the gap starts, each account holding the market whose equity at its marks is below that
//! requirement is closed out on the markets whose gaps start then, largest loser first, at their
//! marks, as a close-out does, until it meets it. Close-outs on price moves keep the normal margin
//! throughout. A listed market's weekend margin moves only as a listing notice of at least seven
//! days names it (`listing_bounds`), or, while rotation is on, as a passed vote does (`governance`),
//! and its calendar changes only as `schedule` allows.

mod schedule;

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Not;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::{Engine, MarketState};
use crate::event::Event;
use crate::execution::{Exec, PriceGuard};
use crate::fixed::Rounding;
use crate::types::{AccountId, MarketId, Millis, Side};

/// How long before a weekend gap starts its weekend margin applies: two hours.
pub const WEEKEND_LEAD_MS: Millis = 2 * 60 * 60 * 1000;

/// A market's next or current weekend gap, or a frontier (`WeekendGap::is_frontier`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeekendGap {
    /// When the gap starts, and its close-out runs: the weekend margin applies from `WEEKEND_LEAD_MS`
    /// before it. For a gap a relisting or an activation found the market already in, when that ran.
    pub close: Millis,
    /// When the market reopens, ending the weekend margin; the next gap is found then. A gap still
    /// shut `WEEKEND_SEARCH_MS` after it is looked at, or whose reopening is further than that, has
    /// that as its reopening until it is looked at again.
    pub reopen: Millis,
    /// Whether the close-out at `close` has run. Left out of the encoding while it has not.
    #[serde(default, skip_serializing_if = "Not::not")]
    pub closed_out: bool,
}

/// When a market's weekend margin applies: from `from`, two hours before its weekend gap starts at
/// `close`, to `reopen`. A read-only view, so it takes no `deny_unknown_fields`, as no type on either
/// side of a `flatten` may.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeekendWindow {
    pub weekend_margin_bps: i64,
    pub from: Millis,
    pub close: Millis,
    pub reopen: Millis,
}

/// What an account holding a market with a weekend margin must have over the market's weekend gap:
/// its margin requirement at its marks now, with the weekend margin of each market whose kept window
/// covers the gap's start, or now once it has started. A market that has not yet reopened from its
/// current gap counts at its normal margin until it does, when its next window is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeekendRequirement {
    #[serde(flatten)]
    pub window: WeekendWindow,
    pub required: Fixed,
}

impl WeekendGap {
    /// The gap's next boundary: its close until its close-out runs, then its reopening.
    fn boundary(&self) -> Millis {
        if self.closed_out { self.reopen } else { self.close }
    }

    /// Whether this is no gap but where a search that found none runs again, `WEEKEND_LEAD_MS` before
    /// its end: closing and reopening there, it applies no margin.
    pub fn is_frontier(&self) -> bool {
        self.close == self.reopen
    }
}

impl MarketState {
    /// When the market's weekend margin applies, for a market with one and a gap, not a frontier.
    pub fn weekend_window(&self) -> Option<WeekendWindow> {
        self.weekend.filter(|gap| !gap.is_frontier()).map(|gap| WeekendWindow {
            weekend_margin_bps: self.spec.weekend_margin_bps,
            from: gap.close - WEEKEND_LEAD_MS,
            close: gap.close,
            reopen: gap.reopen,
        })
    }

    /// Whether the market's positions need their weekend margin at `at`: from `WEEKEND_LEAD_MS` before
    /// its weekend gap starts until it reopens.
    pub(crate) fn weekend_margin_applies(&self, at: Millis) -> bool {
        self.weekend_window().is_some_and(|window| window.from <= at && at < window.reopen)
    }
}

impl Engine {
    /// Each market `account` holds with a weekend margin, with what the account must have over its
    /// weekend gap, at its marks now. Empty for an account never credited.
    pub fn weekend_requirements(&self, account: AccountId) -> BTreeMap<MarketId, WeekendRequirement> {
        let Some(acct) = self.accounts.get(&account) else { return BTreeMap::new() };
        let (_, margin) = self.equity_and_margin(acct, None);
        acct.positions
            .keys()
            .filter_map(|id| Some((*id, self.markets.get(id)?.weekend_window()?)))
            .map(|(id, window)| {
                let at = window.close.max(self.now);
                let required = margin + self.weekend_margin_extra(acct, at, at_mark);
                (id, WeekendRequirement { window, required })
            })
            .collect()
    }

    /// What weekend margins add to `acct`'s margin requirement at `at`, each position valued at the
    /// price and FX index `price_of` gives: for each position in a market whose weekend margin
    /// applies then, its weekend margin less its normal margin, each rounded as a requirement is.
    pub(crate) fn weekend_margin_extra(
        &self,
        acct: &Account,
        at: Millis,
        price_of: impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>),
    ) -> Fixed {
        let charge = self.charge_rounding();
        acct.positions
            .iter()
            .filter_map(|(id, pos)| {
                self.markets.get(id).filter(|market| market.weekend_margin_applies(at)).map(|m| (*id, m, pos))
            })
            .map(|(id, market, pos)| {
                let (price, fx) = price_of(id, market, pos);
                let notional = pos.qty.abs().mul_rounded(price, charge);
                let notional = market.spec.quote_currency.to_usd(notional, fx, Rounding::Up);
                market.spec.weekend_margin_for(notional, charge) - market.spec.margin_for(notional, charge)
            })
            .sum()
    }

    /// What weekend margins add to `acct`'s requirement now, at its marks.
    pub(crate) fn weekend_margin_extra_now(&self, acct: &Account) -> Fixed {
        self.weekend_margin_extra(acct, self.now, at_mark)
    }

    /// The next weekend boundary of any market, from `V59`: a gap's start, until its close-out runs,
    /// then its reopening. The corporate timeline walks to it, though its catch-up bound does not
    /// count it.
    pub(crate) fn next_weekend_boundary(&self) -> Option<Millis> {
        self.markets.values().filter_map(|market| market.weekend.map(|gap| gap.boundary())).min()
    }

    /// At `at`: first the start of the weekend gap of each market whose gap starts then and has not
    /// been closed out, where each account holding one of them whose equity at its marks is below its
    /// margin requirement with the weekend margins that apply then is closed out on those markets,
    /// largest loser first, at their marks, until it meets it; then the reopening of each market whose
    /// closed-out gap ends then, which moves its gap on (`Engine::move_weekend_gap_on`).
    pub(crate) fn run_weekend_boundaries(&mut self, at: Millis, events: &mut Vec<Event>) {
        let due = |closed_out: bool| -> BTreeSet<MarketId> {
            self.markets
                .iter()
                .filter(|(_, market)| market.weekend.is_some_and(|g| g.closed_out == closed_out && g.boundary() == at))
                .map(|(id, _)| *id)
                .collect()
        };
        let (closing, reopening) = (due(false), due(true));
        self.weekend_close_outs(&closing, at, events);
        for market in closing {
            let gap = self.markets.get_mut(&market).expect("listed").weekend.as_mut().expect("selected above");
            gap.closed_out = true;
        }
        for market in reopening {
            self.move_weekend_gap_on(market, at);
        }
    }

    /// Closes out, at `at`, each account holding a market in `closing` whose equity is below its
    /// requirement with the weekend margins that apply then (`Engine::weekend_close_out`).
    fn weekend_close_outs(&mut self, closing: &BTreeSet<MarketId>, at: Millis, events: &mut Vec<Event>) {
        let holders = &self.hot_indexes.as_ref().expect("apply builds derived indexes").holders;
        let accounts: BTreeSet<AccountId> =
            closing.iter().flat_map(|id| holders.get(id).into_iter().flatten().copied()).collect();
        for account in accounts {
            self.weekend_close_out(account, closing, at, events);
        }
    }

    /// Closes `account`'s positions in `closing`, largest loser first, at their marks, while its
    /// equity is below its margin requirement with the weekend margins that apply at `at`, then
    /// settles any bad debt.
    fn weekend_close_out(
        &mut self,
        account: AccountId,
        closing: &BTreeSet<MarketId>,
        at: Millis,
        events: &mut Vec<Event>,
    ) {
        let mut announced = false;
        while let Some((market, qty, equity, margin)) = self.next_weekend_closeout(account, closing, at) {
            if !announced {
                events.push(Event::CloseOut { account, equity, margin });
                announced = true;
            }
            let (mark, fx_mark) = {
                let state = &self.markets[&market];
                (state.mark.expect("only a marked market is closed out"), state.fx_mark)
            };
            let order_id = self.alloc_order_id();
            let side = if qty.is_positive() { Side::Sell } else { Side::Buy };
            let exec = Exec {
                account,
                order_id,
                market,
                side,
                qty: qty.abs(),
                reduce_only: true,
                guard: PriceGuard::None,
                liquidation: true,
            };
            match self.execute_at(exec, mark, fx_mark) {
                Ok(fill) => self.after_fill(fill, events),
                Err(_) => break,
            }
        }
        self.settle_bad_debt(account, events);
    }

    /// The position in `closing` to close next, with the account's equity and its margin requirement
    /// with the weekend margins that apply at `at`, both at its marks, or `None` when the equity
    /// meets the requirement or no position in `closing` can be valued: one in a market never marked,
    /// with no FX mark to convert at, or blocked by a failed split.
    fn next_weekend_closeout(
        &self,
        account: AccountId,
        closing: &BTreeSet<MarketId>,
        at: Millis,
    ) -> Option<(MarketId, Fixed, Fixed, Fixed)> {
        let acct = self.accounts.get(&account)?;
        let (equity, margin) = self.equity_and_margin(acct, None);
        let required = margin + self.weekend_margin_extra(acct, at, at_mark);
        if equity >= required {
            return None;
        }
        acct.positions
            .iter()
            .filter(|(id, _)| closing.contains(id))
            .filter_map(|(id, pos)| {
                let market = &self.markets[id];
                let currency = market.spec.quote_currency;
                let unconvertible = currency.fx_market().is_some() && market.fx_mark.is_none();
                if market.corporate_action_blocked || unconvertible {
                    return None;
                }
                let pnl = currency.to_usd(
                    pos.unrealised(market.mark?, self.protocol_version),
                    market.fx_mark,
                    Rounding::Down,
                );
                Some((pnl, *id, pos.qty))
            })
            .min_by_key(|(pnl, id, _)| (*pnl, *id))
            .map(|(_, id, qty)| (id, qty, equity, required))
    }
}

/// A position's price and FX index at its market's marks, or its entry before the market is marked.
fn at_mark(_: MarketId, market: &MarketState, pos: &Position) -> (Fixed, Option<Fixed>) {
    (market.mark.unwrap_or(pos.entry), market.fx_mark)
}
