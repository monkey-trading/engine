//! What the account read needs of portfolio margin at marks: an account's equity and the requirement
//! close-outs read, and how its close-out slack moves with one grouped position's price, the others held
//! at their marks, from which it estimates that position's close-out price.

use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::Engine;
use crate::fixed::Rounding;
use crate::portfolio::exposure::Exposure;
use crate::portfolio::{Basis, GroupMember, OffsetGroup, Valued, at_mark};
use crate::types::MarketId;

/// An account's close-out slack, equity less the close-out level of its portfolio requirement, as one
/// grouped position's price moves and every other position stays at its mark.
pub struct PortfolioMove<'e> {
    engine: &'e Engine,
    group: &'e OffsetGroup,
    member: &'e GroupMember,
    position: Position,
    /// The group's exposure from its other held members, at their marks.
    others: Exposure,
    /// Equity at the marks less the position's PnL there.
    equity_rest: Fixed,
    /// The requirement at the marks less the position's band and its group's credit there.
    requirement_rest: Fixed,
    level_bps: i64,
}

impl Engine {
    /// `acct`'s equity and the requirement its close-out reads, at marks now: from `V81`, while a grid
    /// applies, its portfolio requirement at the market's bands, and otherwise its per-position
    /// requirement (`Engine::equity_and_margin`).
    pub fn equity_and_closeout_requirement(&self, acct: &Account) -> (Fixed, Fixed) {
        match self.portfolio_rules() {
            Some(grid) => self.equity_and_portfolio_requirement(acct, grid, Basis::Market, &[], at_mark),
            None => self.equity_and_margin(acct, None),
        }
    }

    /// How `acct`'s close-out slack moves with its position in `market`, others at their marks, while a
    /// grid applies and holds `market` in a group whose held members `acct` holds on both sides; `None`
    /// otherwise, or before parameters are set, or for a market `acct` holds no position in. A group held
    /// on one side matches nothing at any price, so its credit is none and the position's requirement
    /// is its bands, as an ungrouped position's is.
    pub fn portfolio_move(&self, acct: &Account, market: MarketId) -> Option<PortfolioMove<'_>> {
        let grid = self.portfolio_rules()?;
        let group = &grid.groups[grid.group_of(market)?];
        let member = group.members.iter().find(|member| member.market == market)?;
        let level_bps = self.params.as_ref()?.closeout_level_bps;
        let position = *acct.positions.get(&market)?;
        let state = self.markets.get(&market)?;
        let held: Vec<(&GroupMember, Valued)> = (group.members.iter())
            .filter_map(|member| {
                let (state, pos) = (self.markets.get(&member.market)?, acct.positions.get(&member.market)?);
                Some((member, self.valued_at(member.market, pos, at_mark(member.market, state, pos).0)))
            })
            .collect();
        // A loading-weighted exposure's side is the sign of quantity times loading: prices are positive.
        let short =
            |(member, valued): &(&GroupMember, Valued)| valued.qty.is_negative() != member.loading.is_negative();
        if held.iter().all(short) || !held.iter().any(short) {
            return None;
        }
        let (equity, requirement) = self.equity_and_portfolio_requirement(acct, grid, Basis::Market, &[], at_mark);
        let (mut others, mut whole) = (Exposure::default(), Exposure::default());
        let mut own_band = Fixed::ZERO;
        for (held_member, valued) in &held {
            whole.add(self.leg(held_member, valued));
            if held_member.market == market {
                own_band = valued.band;
            } else {
                others.add(self.leg(held_member, valued));
            }
        }
        let credit = whole.credit(group.floor_share_bps, self.grid_charge_scale());
        let (price, fx) = at_mark(market, state, &position);
        let pnl =
            state.spec.quote_currency.to_usd(position.unrealised(price, self.protocol_version), fx, Rounding::Down);
        Some(PortfolioMove {
            engine: self,
            group,
            member,
            position,
            others,
            equity_rest: equity - pnl,
            requirement_rest: requirement - own_band + credit,
            level_bps,
        })
    }

    /// `pos` in `market` as the requirement values it at `price`, its market's FX mark converting it.
    fn valued_at(&self, market: MarketId, pos: &Position, price: Fixed) -> Valued {
        let state = &self.markets[&market];
        let notional = pos.qty.abs().mul_rounded(price, self.charge_rounding());
        let notional = state.spec.quote_currency.to_usd(notional, state.fx_mark, Rounding::Up);
        Valued { market, qty: pos.qty, price, notional, band: self.band_charge(state, notional, Basis::Market) }
    }
}

impl PortfolioMove<'_> {
    /// The account's close-out slack with the position at `price` and every other at its mark.
    pub fn slack_at(&self, price: Fixed) -> Fixed {
        let engine = self.engine;
        let own = engine.valued_at(self.member.market, &self.position, price);
        let state = &engine.markets[&self.member.market];
        let pnl = self.position.unrealised(price, engine.protocol_version);
        let pnl = state.spec.quote_currency.to_usd(pnl, state.fx_mark, Rounding::Down);
        let mut exposure = self.others.clone();
        exposure.add(engine.leg(self.member, &own));
        let credit = exposure.credit(self.group.floor_share_bps, engine.grid_charge_scale());
        let requirement = self.requirement_rest + own.band - credit;
        self.equity_rest + pnl - requirement.mul_bps_rounded(self.level_bps, engine.charge_rounding())
    }
}
