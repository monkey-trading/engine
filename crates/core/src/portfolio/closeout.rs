//! A close-out under portfolio margin, from `V81` (#913, D6): close the least that restores the level,
//! keeping each offset group together.
//!
//! The step runs in one of two contexts, which fix its threshold, the prices it values and fills at and
//! the positions it may close, for the scoring and the stopping test alike:
//! - **A price move,** and every other check but a weekend gap's start: the close-out level of the
//!   requirement at the market's bands, each position valued at the index a close-out fills at, or at
//!   its last mark while its market cannot trade; positions in markets that can trade now may close.
//! - **A weekend gap's start:** the requirement in full with the weekend margins (`Basis::Weekend`), at
//!   the marks; positions in the closing markets that the weekend close-out can fill at their marks may
//!   close.
//!
//! Slack is equity less the threshold. A position may close when its market can and its close prices. The
//! candidates are each ungrouped position that may close, each group whose held members may all close,
//! whole, and each member that may close of a group with one that may not, alone. A candidate's gain is the
//! change in slack from closing it at its fill prices: the index or mark, the spread a close-out pays up to
//! the close-spread ceiling, and the profit fee on a realised gain. Each close is scored at the stress
//! level its fill would pay: on a price move the one the fill's own stress refresh would set
//! (`Engine::projected_stress_level`), at a weekend gap's start the one its close-out pays, each at the
//! house's NAV when it fills: the NAV now for a candidate's first close, and for a group's later members
//! the NAV its earlier fills leave, up by what the account gives up on each. If any candidate leaves slack
//! at or above zero, the one closing the least USD notional closes; otherwise the one gaining the most per
//! dollar closed, compared exactly, even when every gain is negative; ties go to the lowest market id, a
//! group's lowest member's. A group closes member by member in market order and finishes before the account
//! is checked again; a fill that fails ends the account's close-out, as before `V81`. Each step closes at
//! least one whole position, so the close-out ends within the account's position count. The rule is applied
//! step by step and searches no combination of candidates.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::I256;

use crate::Fixed;
use crate::account::{Account, Position};
use crate::closeout_index::Valuation;
use crate::command::Command;
use crate::engine::{Engine, MarketState};
use crate::event::Event;
use crate::execution::{Exec, PriceGuard, profit_fee};
use crate::fixed::Rounding;
use crate::levels::FillCause;
use crate::portfolio::{Basis, PortfolioGrid, at_mark};
use crate::types::{AccountId, MarketId, Millis, Side};

/// Where a portfolio close-out runs.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CloseoutContext<'a> {
    /// A price move, and every other check but a weekend gap's start, at the close-out level `level_bps`.
    Price { level_bps: i64 },
    /// A weekend gap's start at `at` over the markets in `closing`.
    Weekend { at: Millis, closing: &'a BTreeSet<MarketId> },
}

/// One step of a portfolio close-out: the positions it closes, in market order, with the account's
/// equity and requirement before it.
#[derive(Debug)]
pub(crate) struct PortfolioStep {
    pub(crate) members: Vec<(MarketId, Position)>,
    pub(crate) equity: Fixed,
    pub(crate) requirement: Fixed,
}

/// A candidate's score: the change in slack its fills make, the USD notional they close, and the
/// market it is known by, its lowest.
struct Scored {
    gain: Fixed,
    notional: Fixed,
    first: MarketId,
}

impl CloseoutContext<'_> {
    /// The requirement this context measures against.
    fn basis(&self) -> Basis {
        match *self {
            CloseoutContext::Price { .. } => Basis::Market,
            CloseoutContext::Weekend { at, .. } => Basis::Weekend { at },
        }
    }

    /// The equity `requirement` asks for here: its close-out level's share on a price move, all of it at a
    /// weekend gap's start.
    fn threshold(&self, requirement: Fixed, charge: Rounding) -> Fixed {
        match self {
            CloseoutContext::Price { level_bps } => requirement.mul_bps_rounded(*level_bps, charge),
            CloseoutContext::Weekend { .. } => requirement,
        }
    }
}

impl Engine {
    /// Closes `account` out in `ctx`, step by step (`next_portfolio_step`), then settles any bad debt.
    pub(crate) fn close_out_portfolio(
        &mut self,
        account: AccountId,
        ctx: CloseoutContext<'_>,
        events: &mut Vec<Event>,
    ) {
        let mut announced = false;
        'steps: while let Some(step) = self.next_portfolio_step(account, ctx) {
            if !announced {
                events.push(Event::CloseOut { account, equity: step.equity, margin: step.requirement });
                announced = true;
            }
            for (market, pos) in step.members {
                let order_id = self.alloc_order_id();
                let exec = Exec {
                    account,
                    order_id,
                    market,
                    side: if pos.qty.is_positive() { Side::Sell } else { Side::Buy },
                    qty: pos.qty.abs(),
                    reduce_only: true,
                    guard: PriceGuard::None,
                    liquidation: true,
                };
                let filled = match ctx {
                    CloseoutContext::Price { .. } => self.execute(exec, events).map(|fill| (fill, FillCause::CloseOut)),
                    CloseoutContext::Weekend { .. } => {
                        let state = &self.markets[&market];
                        let (mark, fx_mark) =
                            (state.mark.expect("only a marked market closes at a gap"), state.fx_mark);
                        self.execute_at(exec, mark, fx_mark).map(|fill| (fill, FillCause::WeekendCloseOut))
                    }
                };
                match filled {
                    Ok((fill, cause)) => self.after_fill(fill, cause, events),
                    Err(_) => break 'steps,
                }
            }
        }
        self.settle_bad_debt(account, events);
    }

    /// The positions `account` closes next in `ctx` under the grid close-outs read, with its equity and
    /// requirement, or `None` when it meets the threshold, its requirement is zero, nothing it holds may
    /// close, or no grid applies.
    pub(crate) fn next_portfolio_step(&self, account: AccountId, ctx: CloseoutContext<'_>) -> Option<PortfolioStep> {
        let grid = self.portfolio_rules()?;
        let acct = self.accounts.get(&account)?;
        let closable = self.closable(acct, ctx);
        let price_of = |id: MarketId, market: &MarketState, pos: &Position| match ctx {
            CloseoutContext::Price { .. } => Valuation::at(market, closable.get(&id).copied()).price_for(pos),
            CloseoutContext::Weekend { .. } => at_mark(id, market, pos),
        };
        let charge = self.charge_rounding();
        let parts = self.requirement_parts(acct, grid, ctx.basis(), price_of);
        let (equity, requirement) = (parts.equity, self.requirement_without(grid, ctx.basis(), &parts, &[]));
        let threshold = ctx.threshold(requirement, charge);
        if requirement.is_zero() || equity >= threshold {
            return None;
        }
        let slack = equity - threshold;
        let share = self.best_price_share(account);
        // The stress level a fill in a market pays at a house NAV: on a price move the level its own stress
        // refresh sets first, at a weekend gap's start the level a weekend close-out pays.
        let level_at = |id: MarketId, nav: Fixed| match ctx {
            CloseoutContext::Price { .. } => self.projected_stress_level(id, nav),
            CloseoutContext::Weekend { .. } => self.close_out_stress_level_at(id, nav),
        };
        // A position's change in equity from closing it at its fill price at a stress level, and its
        // notional; `None` when its close does not price.
        let close = |id: MarketId, level: usize| {
            let (market, pos, (mid, fx)) = (&self.markets[&id], acct.positions[&id], closable[&id]);
            let exec = Exec {
                account,
                order_id: 0,
                market: id,
                side: if pos.qty.is_positive() { Side::Sell } else { Side::Buy },
                qty: pos.qty.abs(),
                reduce_only: true,
                guard: PriceGuard::None,
                liquidation: true,
            };
            let priced = self.price_with(&exec, pos, mid, fx, level, share, 0).ok()?;
            let usd = |amount, fx, rounding| market.spec.quote_currency.to_usd(amount, fx, rounding);
            let mut closed = pos;
            let realised =
                usd(closed.apply(priced.delta, priced.price, self.protocol_version), priced.fx, Rounding::Down);
            let (_, fee_bps) = self.close_fee_terms(account, id);
            let fee = profit_fee(realised, fee_bps, self.protocol_version);
            let (price, valued_fx) = price_of(id, market, &pos);
            let unrealised = usd(pos.unrealised(price, self.protocol_version), valued_fx, Rounding::Down);
            let notional = usd(pos.qty.abs().mul_rounded(price, charge), valued_fx, Rounding::Up);
            Some((realised - fee - unrealised, notional))
        };
        // Each position that may close, at the level its fill pays at the house's NAV now; one whose close
        // does not price waits, as one whose market cannot trade does.
        let nav = self.house_nav();
        let closes: BTreeMap<MarketId, (usize, Fixed, Fixed)> = (closable.keys())
            .filter_map(|id| {
                let level = level_at(*id, nav);
                close(*id, level).map(|(change, notional)| (*id, (level, change, notional)))
            })
            .collect();
        // A group's later members fill at the NAV its earlier fills leave, which takes what the account
        // gives up on each; a candidate one of whose closes does not price there is none.
        let scored = candidates(acct, grid, &closes).into_iter().filter_map(|members| {
            let (mut gain, mut notional, mut after_nav) = (Fixed::ZERO, Fixed::ZERO, nav);
            for (index, id) in members.iter().enumerate() {
                let (level, change, closed) = closes[id];
                let at = if index == 0 { level } else { level_at(*id, after_nav) };
                let (change, closed) = if at == level { (change, closed) } else { close(*id, at)? };
                (gain, notional, after_nav) = (gain + change, notional + closed, after_nav - change);
            }
            let after = self.requirement_without(grid, ctx.basis(), &parts, &members);
            gain += threshold - ctx.threshold(after, charge);
            Some((Scored { gain, notional, first: members[0] }, members))
        });
        let restores = |scored: &Scored| !(slack + scored.gain).is_negative();
        let (_, members) =
            scored.reduce(|best, next| if preferred(&next.0, &best.0, restores) { next } else { best })?;
        let members = members.into_iter().map(|id| (id, acct.positions[&id])).collect();
        Some(PortfolioStep { members, equity, requirement })
    }

    /// The price and FX index each position of `acct` that may close in `ctx` fills at: the live index
    /// of a market that can trade now on a price move; at a weekend gap's start the mark of a closing
    /// market that is marked, convertible and not blocked by a failed split.
    fn closable(&self, acct: &Account, ctx: CloseoutContext<'_>) -> BTreeMap<MarketId, (Fixed, Option<Fixed>)> {
        acct.positions
            .keys()
            .filter_map(|id| {
                let fill = match ctx {
                    CloseoutContext::Price { .. } => self.tradeable_index(*id)?,
                    CloseoutContext::Weekend { closing, .. } => {
                        let market = self.markets.get(id).filter(|_| closing.contains(id))?;
                        let unconvertible =
                            market.spec.quote_currency.fx_market().is_some() && market.fx_mark.is_none();
                        if market.corporate_action_blocked || unconvertible {
                            return None;
                        }
                        (market.mark?, market.fx_mark)
                    }
                };
                Some((*id, fill))
            })
            .collect()
    }
}

impl Engine {
    /// From `V81`, while a grid applies, notes `account` for the check once the command's fills are done,
    /// for a fill of any cause but a close-out's (#913, D7 and D14): a triggered stop or limit, a level,
    /// a close-all entry and an order alike.
    pub(crate) fn note_filled(&mut self, account: AccountId, cause: FillCause) {
        let closing_out = matches!(cause, FillCause::CloseOut | FillCause::WeekendCloseOut);
        if !closing_out && self.portfolio_rules().is_some() {
            self.filled_accounts.insert(account);
        }
    }

    /// Checks each account noted since the last check (`note_filled`) for close-out, in account order, as
    /// a price's check does, whatever markets it still holds: closing one leg can leave an account below
    /// its level even when it lowers the requirement, through the spread and the profit fee on a winning
    /// leg, and a price's check visits only the holders of its market. Runs once a command's own fills
    /// are done, never between the members of a close-out group, and on every return of `Engine::apply`
    /// that may follow a fill, so the set never outlives its command: a snapshot holds none. An admin
    /// `cmd`'s outcome, its last event so far, stays its last event; with no `cmd`, the outcome is still
    /// to come.
    pub(crate) fn check_filled_accounts(&mut self, cmd: Option<&Command>, events: &mut Vec<Event>) {
        let accounts = std::mem::take(&mut self.filled_accounts);
        let level_bps = self.params.as_ref().map(|params| params.closeout_level_bps);
        let Some(level_bps) = level_bps.filter(|_| !accounts.is_empty() && self.portfolio_rules().is_some()) else {
            return;
        };
        let admin = matches!(cmd, Some(Command::Admin(_) | Command::SignedAdmin(_) | Command::ApplyProposal(_)));
        let outcome = if admin { events.pop() } else { None };
        for account in accounts {
            self.close_out_portfolio(account, CloseoutContext::Price { level_bps }, events);
        }
        events.extend(outcome);
    }
}

/// The candidates of `acct` under `grid`, each the markets it closes in market order: each ungrouped
/// position in `closable`, each group whose held members are all in it, whole, and each member in it
/// of a group with a held member that is not, alone.
fn candidates<V>(acct: &Account, grid: &PortfolioGrid, closable: &BTreeMap<MarketId, V>) -> Vec<Vec<MarketId>> {
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    for id in acct.positions.keys() {
        let Some(group) = grid.group_of(*id) else {
            if closable.contains_key(id) {
                found.push(vec![*id]);
            }
            continue;
        };
        if !seen.insert(group) {
            continue;
        }
        let mut held: Vec<MarketId> = (grid.groups[group].members.iter())
            .map(|member| member.market)
            .filter(|market| acct.positions.contains_key(market))
            .collect();
        held.sort_unstable();
        if held.iter().all(|market| closable.contains_key(market)) {
            found.push(held);
        } else {
            found.extend(held.into_iter().filter(|market| closable.contains_key(market)).map(|market| vec![market]));
        }
    }
    found
}

/// Whether `a` is preferred to `b`: among candidates that restore the level, the one closing the least
/// notional; a restoring one over one that does not; otherwise the greater gain per dollar closed,
/// compared by cross-multiplying; then the lower market id.
fn preferred(a: &Scored, b: &Scored, restores: impl Fn(&Scored) -> bool) -> bool {
    let order = match (restores(a), restores(b)) {
        (true, true) => b.notional.cmp(&a.notional),
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let wide = |value: Fixed| I256::try_from(value.raw()).expect("an i128 fits in 256 bits");
            (wide(a.gain) * wide(b.notional)).cmp(&(wide(b.gain) * wide(a.notional)))
        }
    };
    order.then(b.first.cmp(&a.first)) == Ordering::Greater
}
