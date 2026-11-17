//! An account's portfolio requirement at marks now, at the market's bands and at initial bands, and
//! the account read's standing. Each position's notional and band charge are the ones the
//! per-position requirement takes (`Engine::equity_and_margin`), so with no group the requirement is
//! that one exactly.

use std::collections::BTreeMap;

use alloy_primitives::U256;
use serde::Serialize;

use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::{Engine, MarketState};
use crate::fixed::Rounding;
use crate::portfolio::{OffsetGroup, PortfolioGrid};
use crate::types::{AccountId, MarketId};

/// The raw value of a product of three `Fixed` values is at `Fixed`'s scale cubed: this over it
/// brings it to `Fixed`'s.
const SCALE_SQUARED: u64 = 1_000_000_000_000_000_000;
const BPS: u64 = 10_000;

/// An account's portfolio requirement under a grid at marks now, as the account read gives it, with
/// the groups behind it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PortfolioStanding {
    /// R at the market's bands, with each group's normal shocks: the portfolio counterpart of the
    /// per-position requirement.
    pub margin: Fixed,
    /// R at initial bands: each band charged at least the account's tier floor and, over its market's
    /// weekend window, its weekend margin; each group's weekend shocks over its window, and its
    /// members' bands while one the account holds cannot trade; and at least the tier floor on the sum
    /// of the account's notionals.
    pub initial_margin: Fixed,
    /// The close-out level's share of `margin`.
    pub maintenance_margin: Fixed,
    /// Each group the account holds a member of, by name, at the market's bands.
    pub groups: BTreeMap<String, GroupStanding>,
}

/// One offset group's part of an account's portfolio requirement at the market's bands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GroupStanding {
    /// The group's markets the account holds, in the grid's order.
    pub members: Vec<MarketId>,
    /// Σ β N over them, its magnitude rounded up.
    pub net_notional: Fixed,
    /// L: the worst loss over the grid, `F × |Σ β N| + Σ ρ |N|`.
    pub worst_loss: Fixed,
    /// Which way the factor's move gives the worst loss.
    pub binding_direction: FactorMove,
    /// What the group takes off the per-position requirement: its members' bands less its worst loss
    /// and the cost of closing its members; negative where they come to more than the bands.
    pub offset: Fixed,
}

/// Which way an offset group's factor moves in its worst loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FactorMove {
    /// A net long: the factor falling.
    Down,
    /// A net short: the factor rising.
    Up,
    /// No net: either way, the residuals alone.
    Either,
}

/// The bands, shocks and rules a requirement takes.
#[derive(Clone, Copy)]
enum Bands {
    /// The market's bands, and each group's normal shocks.
    Market,
    /// Each band at least `floor_bps`, the account's tier floor, and over its market's weekend window
    /// its weekend margin; each group's weekend shocks over its window, and its members' bands while
    /// one the account holds cannot trade.
    Initial { floor_bps: i64 },
}

/// One offset group's terms for an account. Wide amounts are at `Fixed`'s scale.
#[derive(Default)]
struct GroupTerms {
    /// Σ β N.
    net: Net,
    /// L: the worst loss over the grid.
    loss: U256,
    /// The band charges of the group's positions.
    bands: Fixed,
    /// The cost of closing the group's positions at their ceilings, over the close-out level.
    closing: U256,
    /// Whether the account holds a member that cannot trade now; set at initial bands only.
    shut: bool,
}

/// Σ β × qty × price over a group's positions, exact: each term is formed whole, positives and
/// negatives summed apart, and only the result's magnitude rounds. Each term is at most
/// |qty × price| × |β| at `Fixed`'s scale cubed, where `Engine::notional_at_mark` has already formed
/// |qty × price| as a `Fixed` (halting outside its range, as every margin read does), so it is below
/// 2^127 × 10^9 < 2^157; |β| is at most `MAX_LOADING`, 10^10 < 2^34 raw; and a group holds at most
/// one member per market id, 2^32. Each sum stays below 2^223, inside `U256`.
#[derive(Default)]
pub(super) struct Net {
    long: U256,
    short: U256,
}

impl Net {
    /// Adds `loading` × `qty` × `price`.
    pub(super) fn add(&mut self, qty: Fixed, price: Fixed, loading: Fixed) {
        let magnitude = [qty, price, loading]
            .iter()
            .fold(U256::from(1), |product, factor| product.saturating_mul(U256::from(factor.raw().unsigned_abs())));
        let negative = qty.is_negative() ^ price.is_negative() ^ loading.is_negative();
        let side = if negative { &mut self.short } else { &mut self.long };
        *side = side.saturating_add(magnitude);
    }

    /// Whether the net is below zero, and its magnitude at `Fixed`'s scale, rounded up.
    pub(super) fn rounded(&self) -> (bool, U256) {
        let short = self.short > self.long;
        let magnitude = if short { self.short - self.long } else { self.long - self.short };
        (short, ceil_div(magnitude, U256::from(SCALE_SQUARED)))
    }
}

impl Engine {
    /// `acct`'s portfolio requirement under `grid` at the market's bands, at marks now: R with each
    /// group's normal shocks, never above the per-position requirement `Engine::equity_and_margin`
    /// gives. `grid` is one `PortfolioGrid::validate` and `PortfolioGrid::check_listings` passed.
    pub fn portfolio_margin(&self, acct: &Account, grid: &PortfolioGrid) -> Fixed {
        self.portfolio_requirement(acct, grid, Bands::Market)
    }

    /// `account`'s portfolio requirement under `grid` at marks now, at the market's bands and at
    /// initial bands, with each group it holds a member of; `None` for an account never credited.
    /// Error cases:
    /// - `grid` does not fit the listings (see `PortfolioGrid::check_listings`)
    pub fn portfolio_standing(
        &self,
        account: AccountId,
        grid: &PortfolioGrid,
    ) -> Result<Option<PortfolioStanding>, String> {
        grid.check_listings(|id| self.markets.get(&id).map(|market| &market.spec))?;
        let Some(acct) = self.accounts.get(&account) else { return Ok(None) };
        let margin = self.portfolio_requirement(acct, grid, Bands::Market);
        let floor_bps = self.margin_floor_bps(account);
        let initial_margin = self.portfolio_requirement(acct, grid, Bands::Initial { floor_bps });
        let maintenance_margin = match &self.params {
            Some(params) => margin.mul_bps_rounded(params.closeout_level_bps, self.charge_rounding()),
            None => margin,
        };
        let groups = grid
            .groups
            .iter()
            .filter_map(|group| {
                let members: Vec<MarketId> =
                    group.members.iter().map(|member| member.market).filter(|id| self.holds(acct, *id)).collect();
                (!members.is_empty()).then(|| (group.name.clone(), self.group_standing(acct, group, members)))
            })
            .collect();
        Ok(Some(PortfolioStanding { margin, initial_margin, maintenance_margin, groups }))
    }

    /// `acct`'s portfolio requirement under `grid` at `bands`: `min(PP, max(φ × PP, M))`, and at initial
    /// bands at least the tier floor on the sum of its notionals.
    fn portfolio_requirement(&self, acct: &Account, grid: &PortfolioGrid, bands: Bands) -> Fixed {
        let (mut per_position, mut model, mut gross) = (Fixed::ZERO, U256::ZERO, U256::ZERO);
        for (id, pos) in &acct.positions {
            let Some(market) = self.markets.get(id) else { continue };
            let (_, notional) = self.notional_at_mark(market, pos);
            let charge = self.band_charge(market, notional, bands);
            per_position += charge;
            gross = gross.saturating_add(wide(notional));
            if grid.group_of(*id).is_none() {
                model = model.saturating_add(wide(charge)).saturating_add(self.closing_cost(*id, notional));
            }
        }
        for group in &grid.groups {
            let terms = self.group_terms(acct, group, bands);
            let risk = if terms.shut { wide(terms.bands) } else { terms.loss };
            model = model.saturating_add(risk).saturating_add(terms.closing);
        }
        let floor = per_position.mul_bps_rounded(grid.floor_share_bps, Rounding::Up);
        // A model past the per-position requirement may be past `Fixed`'s range; one below it is not.
        let requirement = if model >= wide(per_position) { per_position } else { narrow(model).max(floor) };
        match bands {
            Bands::Market => requirement,
            // Every band charges at least the floor, so this stays at or below the per-position
            // requirement.
            Bands::Initial { floor_bps } => {
                let tier_floor = ceil_div(gross.saturating_mul(U256::from(floor_bps.unsigned_abs())), U256::from(BPS));
                requirement.max(narrow(tier_floor))
            }
        }
    }

    /// The terms of `group` in `acct`'s requirement at `bands`.
    fn group_terms(&self, acct: &Account, group: &OffsetGroup, bands: Bands) -> GroupTerms {
        let initial = matches!(bands, Bands::Initial { .. });
        let listed = |id: MarketId| self.markets.get(&id);
        let weekend = initial
            && group
                .members
                .iter()
                .filter_map(|member| listed(member.market))
                .any(|m| m.weekend_margin_applies(self.now));
        let mut terms = GroupTerms::default();
        let mut residual = U256::ZERO;
        for member in &group.members {
            let (Some(market), Some(pos)) = (listed(member.market), acct.positions.get(&member.market)) else {
                continue;
            };
            let (price, notional) = self.notional_at_mark(market, pos);
            terms.bands += self.band_charge(market, notional, bands);
            terms.closing = terms.closing.saturating_add(self.closing_cost(member.market, notional));
            let rho = if weekend { member.weekend_residual_shock_bps } else { member.residual_shock_bps };
            residual = residual.saturating_add(wide(notional.mul_bps_rounded(rho, Rounding::Up)));
            terms.net.add(pos.qty, price, member.loading);
            terms.shut |= initial && !pos.qty.is_zero() && self.tradeable_index(member.market).is_none();
        }
        let factor = if weekend { group.weekend_factor_shock_bps } else { group.factor_shock_bps };
        let (_, net) = terms.net.rounded();
        let factor_loss = ceil_div(net.saturating_mul(U256::from(factor.unsigned_abs())), U256::from(BPS));
        terms.loss = factor_loss.saturating_add(residual);
        terms
    }

    /// `group`'s standing in `acct`'s requirement at the market's bands, `members` the ones it holds.
    fn group_standing(&self, acct: &Account, group: &OffsetGroup, members: Vec<MarketId>) -> GroupStanding {
        let terms = self.group_terms(acct, group, Bands::Market);
        let (short, net) = terms.net.rounded();
        let binding_direction = match (net.is_zero(), short) {
            (true, _) => FactorMove::Either,
            (false, true) => FactorMove::Up,
            (false, false) => FactorMove::Down,
        };
        let net_notional = if short { -narrow(net) } else { narrow(net) };
        let offset = terms.bands - narrow(terms.loss.saturating_add(terms.closing));
        GroupStanding { members, net_notional, worst_loss: narrow(terms.loss), binding_direction, offset }
    }

    /// Whether `acct` holds a position in `market`, a listed one.
    fn holds(&self, acct: &Account, market: MarketId) -> bool {
        self.markets.contains_key(&market) && acct.positions.get(&market).is_some_and(|pos| !pos.qty.is_zero())
    }

    /// A position's price at its market's mark, or its entry before the market is marked, and its USD
    /// notional there, rounded as the per-position requirement rounds it.
    fn notional_at_mark(&self, market: &MarketState, pos: &Position) -> (Fixed, Fixed) {
        let price = market.mark.unwrap_or(pos.entry);
        let notional = pos.qty.abs().mul_rounded(price, self.charge_rounding());
        (price, market.spec.quote_currency.to_usd(notional, market.fx_mark, Rounding::Up))
    }

    /// The per-position requirement of `notional` in `market` at `bands`.
    fn band_charge(&self, market: &MarketState, notional: Fixed, bands: Bands) -> Fixed {
        let floor_bps = match bands {
            Bands::Market => 0,
            Bands::Initial { floor_bps } if market.weekend_margin_applies(self.now) => {
                floor_bps.max(market.spec.weekend_margin_bps)
            }
            Bands::Initial { floor_bps } => floor_bps,
        };
        market.spec.floored_margin_for(notional, floor_bps, self.charge_rounding())
    }

    /// What closing `notional` in `market` at its close-spread ceiling costs, over the close-out level,
    /// rounded up; nothing for a market with no ceiling, or before parameters are set.
    fn closing_cost(&self, market: MarketId, notional: Fixed) -> U256 {
        let (Some(ceiling), Some(params)) = (self.close_spread_ceiling(market), &self.params) else {
            return U256::ZERO;
        };
        let cost = wide(notional).saturating_mul(U256::from(ceiling.unsigned_abs()));
        ceil_div(cost, U256::from(params.closeout_level_bps.unsigned_abs()))
    }
}

/// A non-negative `Fixed` as a wide amount at its scale.
fn wide(amount: Fixed) -> U256 {
    U256::from(amount.raw().unsigned_abs())
}

/// A wide amount as a `Fixed`, at most the largest one: a figure past it reads as it.
fn narrow(amount: U256) -> Fixed {
    Fixed::from_raw(i128::try_from(amount).unwrap_or(i128::MAX))
}

/// `numerator / denominator`, rounded up. Panics on a zero denominator, which callers rule out.
fn ceil_div(numerator: U256, denominator: U256) -> U256 {
    let (quotient, remainder) = numerator.div_rem(denominator);
    if remainder.is_zero() { quotient } else { quotient + U256::from(1) }
}
