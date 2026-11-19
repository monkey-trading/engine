//! An account's portfolio requirement under a basis, each position valued at a price the caller gives:
//! at the market's bands, the one close-outs read from `V81`; at initial bands, for the account read;
//! and at a weekend gap's start, with each market's weekend margin. Each group's credit is the same on
//! every basis but for the bands it takes off. Each position's notional and band charge are the ones the
//! per-position requirement takes (`Engine::equity_and_margin_at`), so a book no group credits keeps
//! that requirement exactly.

use std::collections::BTreeMap;

use alloy_primitives::{U256, U512};
use serde::Serialize;

use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::{Engine, MarketState};
use crate::fixed::Rounding;
use crate::portfolio::exposure::{BPS, Exposure, Leg, narrow, wide};
use crate::portfolio::{GroupMember, OffsetGroup, PortfolioGrid};
use crate::types::{AccountId, MarketId, Millis};

/// An account's portfolio requirement under a grid at marks now, as the account read gives it, with
/// the groups behind it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PortfolioStanding {
    /// The per-position requirement at the market's bands less each group's credit.
    pub margin: Fixed,
    /// The same at initial bands: each band charged at least the account's tier floor and, over its
    /// market's weekend window, its weekend margin; and no credit for a group while a member the account
    /// holds cannot trade.
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

/// The bands a requirement takes. Each group's credit is at its members' residual shocks on every basis.
#[derive(Clone, Copy)]
pub(crate) enum Basis {
    /// The market's bands: the close-out level's share of this is what close-outs read from `V81`, a
    /// member that cannot trade valued at its last mark and credited.
    Market,
    /// Each band at least `floor_bps`, the account's tier floor, and with `weekend` over its market's
    /// weekend window its weekend margin; and no credit while a member the account holds cannot trade,
    /// unless `shut_credited`, when such a group keeps its credit, its member valued as given, as at the
    /// market's bands.
    Initial { floor_bps: i64, weekend: bool, shut_credited: bool },
    /// A weekend gap's start at `at`: each band at least its market's weekend margin while that applies
    /// at `at`, as `V60` charges it.
    Weekend { at: Millis },
}

/// A grouped position as the per-position requirement values it: its quantity, the price it is valued
/// at, its USD notional there and its band charge.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Valued {
    pub(crate) market: MarketId,
    pub(crate) qty: Fixed,
    pub(crate) price: Fixed,
    pub(crate) notional: Fixed,
    pub(crate) band: Fixed,
}

/// An account's portfolio requirement in its parts, as `Engine::requirement_parts` gives them: its equity,
/// each position's band charge in market order, the grouped positions as valued, and each group's credit
/// in grid order.
pub(crate) struct RequirementParts {
    pub(crate) equity: Fixed,
    bands: Vec<(MarketId, Fixed)>,
    grouped: Vec<Valued>,
    credits: Vec<Fixed>,
}

impl Engine {
    /// `acct`'s portfolio requirement under `grid` at the market's bands, at marks now: the
    /// per-position requirement `Engine::equity_and_margin` gives, less each group's credit at each
    /// member's residual shock. `grid` is one `PortfolioGrid::validate` and
    /// `PortfolioGrid::check_listings` passed.
    pub fn portfolio_margin(&self, acct: &Account, grid: &PortfolioGrid) -> Fixed {
        self.equity_and_portfolio_requirement(acct, grid, Basis::Market, &[], at_mark).1
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
        let margin = self.portfolio_margin(acct, grid);
        let floor_bps = self.margin_floor_bps(account);
        let initial = Basis::Initial { floor_bps, weekend: true, shut_credited: false };
        let (_, initial_margin) = self.equity_and_portfolio_requirement(acct, grid, initial, &[], at_mark);
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

    /// `acct`'s equity and portfolio requirement under `grid` at `basis`, with each position but those in
    /// `without` valued at the price and FX index `price_of(market id, market, position)` gives: equity
    /// and the per-position requirement folded in market order as `Engine::equity_and_margin_at` folds
    /// them, PnL converting rounded down and notional rounded up, less each group's credit.
    pub(crate) fn equity_and_portfolio_requirement(
        &self,
        acct: &Account,
        grid: &PortfolioGrid,
        basis: Basis,
        without: &[MarketId],
        price_of: impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>),
    ) -> (Fixed, Fixed) {
        let (equity, per_position, grouped) = self.valued_positions(acct, grid, basis, without, price_of, |_, _| {});
        (equity, per_position - self.portfolio_credit(grid, basis, &grouped))
    }

    /// `equity_and_portfolio_requirement` of `acct` with nothing left out, in its parts, so a close-out can
    /// take the requirement without some of the positions (`requirement_without`) without valuing again.
    pub(crate) fn requirement_parts(
        &self,
        acct: &Account,
        grid: &PortfolioGrid,
        basis: Basis,
        price_of: impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>),
    ) -> RequirementParts {
        let mut bands = Vec::with_capacity(acct.positions.len());
        let (equity, _, grouped) = self.valued_positions(acct, grid, basis, &[], price_of, |id, band| {
            bands.push((id, band));
        });
        let credits = grid.groups.iter().map(|group| self.group_credit(group, basis, &grouped)).collect();
        RequirementParts { equity, bands, grouped, credits }
    }

    /// The requirement `parts` give, less the positions in `without`: exactly what
    /// `equity_and_portfolio_requirement` gives without them, since the per-position requirement is the
    /// sum of the bands and a group's credit reads only its own members. Their bands come off, each group
    /// with a member among them is credited again on the rest, and every other group's credit stands.
    pub(crate) fn requirement_without(
        &self,
        grid: &PortfolioGrid,
        basis: Basis,
        parts: &RequirementParts,
        without: &[MarketId],
    ) -> Fixed {
        let kept = |id: &MarketId| !without.contains(id);
        let per_position: Fixed = parts.bands.iter().filter(|(id, _)| kept(id)).map(|(_, band)| *band).sum();
        let mut rest = None;
        let credit: Fixed = (grid.groups.iter().zip(&parts.credits))
            .map(|(group, credit)| match group.members.iter().any(|member| without.contains(&member.market)) {
                false => *credit,
                true => {
                    let rest = rest.get_or_insert_with(|| {
                        parts.grouped.iter().filter(|valued| kept(&valued.market)).copied().collect::<Vec<_>>()
                    });
                    self.group_credit(group, basis, rest)
                }
            })
            .sum();
        per_position - credit
    }

    /// `acct`'s equity, per-position requirement and grouped positions as valued under `grid` at `basis`,
    /// each position but those in `without` valued at the price and FX index `price_of` gives, each one's
    /// band charge passed to `band_of`.
    fn valued_positions(
        &self,
        acct: &Account,
        grid: &PortfolioGrid,
        basis: Basis,
        without: &[MarketId],
        price_of: impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>),
        mut band_of: impl FnMut(MarketId, Fixed),
    ) -> (Fixed, Fixed, Vec<Valued>) {
        let charge = self.charge_rounding();
        let (mut equity, mut per_position) = (acct.balance, Fixed::ZERO);
        let mut grouped = Vec::new();
        for (id, pos) in acct.positions.iter().filter(|(id, _)| !without.contains(id)) {
            let Some(market) = self.markets.get(id) else { continue };
            let (price, fx) = price_of(*id, market, pos);
            let usd = |amount, rounding| market.spec.quote_currency.to_usd(amount, fx, rounding);
            let notional = usd(pos.qty.abs().mul_rounded(price, charge), Rounding::Up);
            let band = self.band_charge(market, notional, basis);
            equity += usd(pos.unrealised(price, self.protocol_version), Rounding::Down);
            per_position += band;
            band_of(*id, band);
            if grid.group_of(*id).is_some() {
                grouped.push(Valued { market: *id, qty: pos.qty, price, notional, band });
            }
        }
        (equity, per_position, grouped)
    }

    /// What every group of `grid` takes off the per-position requirement at `basis`, given the grouped
    /// positions as the requirement valued them, `grouped`. Each credit is at most its group's share of
    /// the bands, so the requirement less their sum is at least zero.
    pub(crate) fn portfolio_credit(&self, grid: &PortfolioGrid, basis: Basis, grouped: &[Valued]) -> Fixed {
        grid.groups.iter().map(|group| self.group_credit(group, basis, grouped)).sum()
    }

    /// What `group` takes off the per-position requirement at `basis` on the grouped positions `grouped`:
    /// nothing when they hold none of its members.
    fn group_credit(&self, group: &OffsetGroup, basis: Basis, grouped: &[Valued]) -> Fixed {
        self.group_exposure(group, basis, grouped)
            .map_or(Fixed::ZERO, |(group, exposure)| exposure.credit(group.floor_share_bps, self.grid_charge_scale()))
    }

    /// `group`'s loading-weighted exposure at `basis` from the grouped positions `grouped`, or `None` when
    /// they hold none of its members, or at initial bands while one they hold cannot trade.
    fn group_exposure<'g>(
        &self,
        group: &'g OffsetGroup,
        basis: Basis,
        grouped: &[Valued],
    ) -> Option<(&'g OffsetGroup, Exposure)> {
        let held = || {
            group.members.iter().filter_map(|member| {
                grouped.iter().find(|valued| valued.market == member.market).map(|valued| (member, valued))
            })
        };
        let initial = matches!(basis, Basis::Initial { shut_credited: false, .. });
        if held().next().is_none() || initial && held().any(|(member, _)| self.tradeable_index(member.market).is_none())
        {
            return None;
        }
        let mut exposure = Exposure::default();
        for (member, valued) in held() {
            exposure.add(self.leg(member, valued));
        }
        Some((group, exposure))
    }

    /// `valued`'s leg in `member`'s group, at its residual shock.
    pub(super) fn leg(&self, member: &GroupMember, valued: &Valued) -> Leg {
        let rho = member.residual_shock_bps;
        Leg {
            qty: valued.qty,
            price: valued.price,
            loading: member.loading,
            bands: valued.band,
            charge: self.grid_charge(member, valued.notional, rho),
            residual: wide(valued.notional) * U256::from(rho.unsigned_abs()),
        }
    }

    /// `notional`'s grid charge in `member`'s market at a residual shock of `rho` bps: its own move,
    /// `rho / 10^4`, and the cost of closing it at its close-spread ceiling over the close-out level,
    /// `ceiling / level`, both over the common denominator `grid_charge_scale`, so no division rounds it.
    /// A market with no ceiling, or a log before its parameters, adds no closing cost.
    fn grid_charge(&self, member: &GroupMember, notional: Fixed, rho: i64) -> U256 {
        let ceiling = self.params.as_ref().and_then(|_| self.close_spread_ceiling(member.market)).unwrap_or(0);
        let per_unit = U256::from(rho.unsigned_abs()) * U256::from(self.grid_closeout_level())
            + U256::from(ceiling.unsigned_abs()) * U256::from(BPS);
        wide(notional) * per_unit
    }

    /// The scale every grid charge is at over `Fixed`'s: `10^4 × the close-out level` in bps.
    pub(super) fn grid_charge_scale(&self) -> U512 {
        U512::from(BPS) * U512::from(self.grid_closeout_level())
    }

    /// The close-out level in bps, or all of it before parameters are set, when no close costs anything.
    fn grid_closeout_level(&self) -> u64 {
        self.params.as_ref().map_or(BPS, |params| params.closeout_level_bps.unsigned_abs())
    }

    /// `group`'s standing in `acct`'s requirement at the market's bands, `members` the ones it holds.
    fn group_standing(&self, acct: &Account, group: &OffsetGroup, members: Vec<MarketId>) -> GroupStanding {
        let charge = self.charge_rounding();
        let grouped: Vec<Valued> = members
            .iter()
            .map(|id| {
                let (market, pos) = (&self.markets[id], &acct.positions[id]);
                let (price, fx) = at_mark(*id, market, pos);
                let notional =
                    market.spec.quote_currency.to_usd(pos.qty.abs().mul_rounded(price, charge), fx, Rounding::Up);
                let band = self.band_charge(market, notional, Basis::Market);
                Valued { market: *id, qty: pos.qty, price, notional, band }
            })
            .collect();
        let exposure =
            self.group_exposure(group, Basis::Market, &grouped).map(|(_, exposure)| exposure).unwrap_or_default();
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

    /// The per-position requirement of `notional` in `market` at `basis`.
    pub(crate) fn band_charge(&self, market: &MarketState, notional: Fixed, basis: Basis) -> Fixed {
        let floor_bps = match basis {
            Basis::Market => 0,
            Basis::Initial { floor_bps, weekend: true, .. } if market.weekend_margin_applies(self.now) => {
                floor_bps.max(market.spec.weekend_margin_bps)
            }
            Basis::Initial { floor_bps, .. } => floor_bps,
            Basis::Weekend { at } if market.weekend_margin_applies(at) => market.spec.weekend_margin_bps.max(0),
            Basis::Weekend { .. } => 0,
        };
        market.spec.floored_margin_for(notional, floor_bps, self.charge_rounding())
    }
}

/// A position's price and FX index at its market's marks, or its entry before the market is marked.
pub(crate) fn at_mark(_: MarketId, market: &MarketState, pos: &Position) -> (Fixed, Option<Fixed>) {
    (market.mark.unwrap_or(pos.entry), market.fx_mark)
}
