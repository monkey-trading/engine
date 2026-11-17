//! An account's portfolio requirement at marks now, at the market's bands and at initial bands, and
//! the account read's standing. Each position's notional and band charge are the ones the
//! per-position requirement takes (`Engine::equity_and_margin`), so a book no group credits keeps that
//! requirement exactly.

use std::collections::BTreeMap;

use alloy_primitives::U512;
use serde::Serialize;

use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::{Engine, MarketState};
use crate::fixed::Rounding;
use crate::portfolio::exposure::{BPS, Exposure, Leg, narrow, wide};
use crate::portfolio::{GroupMember, OffsetGroup, PortfolioGrid};
use crate::types::{AccountId, MarketId};
use crate::weekend::WEEKEND_LEAD_MS;

/// An account's portfolio requirement under a grid at marks now, as the account read gives it, with
/// the groups behind it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PortfolioStanding {
    /// The per-position requirement at the market's bands less each group's credit, with each
    /// member's normal shocks.
    pub margin: Fixed,
    /// The same at initial bands: each band charged at least the account's tier floor and, over its
    /// market's weekend window, its weekend margin; from two hours before a group's weekend gap until
    /// its members reopen, its weekend shocks, and no credit on the matched part of a market with no
    /// weekend rate; and no credit for a group while a member the account holds cannot trade.
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
    /// Σ β N over them, N each position's signed USD notional; its magnitude rounded up.
    pub net_notional: Fixed,
    /// The matched exposure: the gross loading-weighted exposure less the net's magnitude, twice the
    /// smaller side's; rounded down.
    pub matched_notional: Fixed,
    /// The matched exposure's worst loss over the grid, each matched part's own move ±ρ, the factor's
    /// cancelling on it; rounded up.
    pub worst_loss: Fixed,
    /// Which way the factor's move hurts the exposure left unmatched.
    pub binding_direction: FactorMove,
    /// What the group takes off the per-position requirement: its matched exposure's share of the
    /// bands less its worst loss and the cost of closing it, at most `1 - φ` of that share.
    pub offset: Fixed,
}

/// Which way an offset group's factor moves against its unmatched exposure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FactorMove {
    /// A net long: the factor falling.
    Down,
    /// A net short: the factor rising.
    Up,
    /// No net: every exposure matched.
    Either,
}

/// The bands and shocks a requirement takes.
#[derive(Clone, Copy)]
enum Bands {
    /// The market's bands, and each member's normal shocks.
    Market,
    /// Each band at least `floor_bps`, the account's tier floor, and over its market's weekend window
    /// its weekend margin; over its group's weekend gap and the lead before it, each member's weekend
    /// shocks and no credit on the matched part of a market with no weekend rate; and no credit while
    /// a member the account holds cannot trade.
    Initial { floor_bps: i64 },
}

impl Engine {
    /// `acct`'s portfolio requirement under `grid` at the market's bands, at marks now: the
    /// per-position requirement `Engine::equity_and_margin` gives, less each group's credit at each
    /// member's normal shocks. `grid` is one `PortfolioGrid::validate` and
    /// `PortfolioGrid::check_listings` passed.
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
                let members: Vec<MarketId> = group
                    .members
                    .iter()
                    .map(|member| member.market)
                    .filter(|id| self.holds_listed(acct, *id))
                    .collect();
                (!members.is_empty()).then(|| (group.name.clone(), self.group_standing(acct, group, members)))
            })
            .collect();
        Ok(Some(PortfolioStanding { margin, initial_margin, maintenance_margin, groups }))
    }

    /// `acct`'s portfolio requirement under `grid` at `bands`: the per-position requirement less each
    /// group's credit.
    fn portfolio_requirement(&self, acct: &Account, grid: &PortfolioGrid, bands: Bands) -> Fixed {
        let mut per_position = Fixed::ZERO;
        for (id, pos) in &acct.positions {
            let Some(market) = self.markets.get(id) else { continue };
            let (_, notional) = self.notional_at_mark(market, pos);
            per_position += self.band_charge(market, notional, bands);
        }
        let credit: Fixed = grid
            .groups
            .iter()
            .filter_map(|group| self.group_exposure(acct, group, bands).map(|exposure| (group, exposure)))
            .map(|(group, exposure)| exposure.credit(group.floor_share_bps, self.grid_charge_scale()))
            .sum();
        // Each credit is at most its group's share of the bands, so this is at least zero.
        per_position - credit
    }

    /// `group`'s loading-weighted exposure in `acct` at `bands`, or `None` when the account holds none
    /// of its members, or at initial bands while one it holds cannot trade.
    fn group_exposure(&self, acct: &Account, group: &OffsetGroup, bands: Bands) -> Option<Exposure> {
        let held = || group.members.iter().map(|member| member.market).filter(|id| self.holds_listed(acct, *id));
        let initial = matches!(bands, Bands::Initial { .. });
        // Before the weekend's calendar search, which a group whose markets are shut would discard.
        if held().next().is_none() || initial && held().any(|id| self.tradeable_index(id).is_none()) {
            return None;
        }
        let weekend = initial && self.group_weekend(group);
        let mut exposure = Exposure::default();
        for member in &group.members {
            let (Some(market), Some(pos)) = (self.markets.get(&member.market), acct.positions.get(&member.market))
            else {
                continue;
            };
            let (price, notional) = self.notional_at_mark(market, pos);
            let rho = if weekend { member.weekend_residual_shock_bps } else { member.residual_shock_bps };
            // Over the weekend, the matched part of a market with no weekend rate keeps its bands: it
            // still matches the other side, but brings neither bands nor a charge to the credit.
            let credited = !(weekend && market.spec.weekend_margin_bps == 0);
            exposure.add(Leg {
                qty: pos.qty,
                price,
                loading: member.loading,
                bands: if credited { self.band_charge(market, notional, bands) } else { Fixed::ZERO },
                charge: if credited { self.grid_charge(member, notional, rho) } else { U512::ZERO },
                residual: wide(notional) * U512::from(rho.unsigned_abs()),
            });
        }
        Some(exposure)
    }

    /// `notional`'s grid charge in `member`'s market at a residual shock of `rho` bps: its own move,
    /// `rho / 10^4`, and the cost of closing it at its close-spread ceiling over the close-out level,
    /// `ceiling / level`, both over the common denominator `grid_charge_scale`, so no division rounds it.
    /// A market with no ceiling, or a log before its parameters, adds no closing cost.
    fn grid_charge(&self, member: &GroupMember, notional: Fixed, rho: i64) -> U512 {
        let ceiling = self.params.as_ref().and_then(|_| self.close_spread_ceiling(member.market)).unwrap_or(0);
        let per_unit = U512::from(rho.unsigned_abs()) * U512::from(self.grid_closeout_level())
            + U512::from(ceiling.unsigned_abs()) * U512::from(BPS);
        wide(notional) * per_unit
    }

    /// The scale every grid charge is at over `Fixed`'s: `10^4 × the close-out level` in bps.
    fn grid_charge_scale(&self) -> U512 {
        U512::from(BPS) * U512::from(self.grid_closeout_level())
    }

    /// The close-out level in bps, or all of it before parameters are set, when no close costs anything.
    fn grid_closeout_level(&self) -> u64 {
        self.params.as_ref().map_or(BPS, |params| params.closeout_level_bps.unsigned_abs())
    }

    /// Whether `group`'s weekend shocks apply now: from `WEEKEND_LEAD_MS` before a weekend gap of its
    /// members' shared schedule starts until they reopen, whatever their weekend margins. A member that
    /// keeps a weekend gap holds that window. Otherwise the schedule is searched, minute by minute while
    /// the venue is shut: only within the lead before a closure, since a group whose held members are
    /// shut takes no credit and is not asked. A gap lasts at least `WEEKEND_GAP_MS`, far past the lead,
    /// so one that starts within the lead is under way at its end.
    fn group_weekend(&self, group: &OffsetGroup) -> bool {
        let listed = || group.members.iter().filter_map(|member| self.markets.get(&member.market));
        if listed().any(|market| market.weekend.is_some()) {
            return listed().any(|market| market.weekend_margin_applies(self.now));
        }
        let Some(market) = listed().next() else { return false };
        let lead_end = self.now.saturating_add(WEEKEND_LEAD_MS);
        [self.now, lead_end].into_iter().any(|at| market.spec.schedule.weekend_gap_at(at).is_some())
    }

    /// `group`'s standing in `acct`'s requirement at the market's bands, `members` the ones it holds.
    fn group_standing(&self, acct: &Account, group: &OffsetGroup, members: Vec<MarketId>) -> GroupStanding {
        let exposure = self.group_exposure(acct, group, Bands::Market).unwrap_or_default();
        let (short, net) = exposure.net();
        let binding_direction = match (net.is_zero(), short) {
            (true, _) => FactorMove::Either,
            (false, true) => FactorMove::Up,
            (false, false) => FactorMove::Down,
        };
        GroupStanding {
            members,
            net_notional: if short { -narrow(net) } else { narrow(net) },
            matched_notional: narrow(exposure.matched()),
            worst_loss: narrow(exposure.worst_loss()),
            binding_direction,
            offset: exposure.credit(group.floor_share_bps, self.grid_charge_scale()),
        }
    }

    /// Whether `acct` holds a position in `market`, a listed one.
    fn holds_listed(&self, acct: &Account, market: MarketId) -> bool {
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
}
