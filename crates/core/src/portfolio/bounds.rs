//! How the grid in force moves, from `V81` (#913, D11): its groups, their members and loadings, and
//! each bound are fixed at the launch grid, the grid in force when parameters are first set under
//! `V81` or when it activates over them; a seven-day notice or a passed vote moves only the residual
//! shocks and the floor shares, each from its launch value up to its bound. A changed loading could
//! make a book offset that did not before, so no bound on loadings alone would keep the grid from
//! growing more generous than at launch. A log whose launch grid is empty keeps none.

use std::collections::BTreeMap;

use crate::engine::Engine;
use crate::operator_bounds::ParamsNotice;
use crate::portfolio::PortfolioGrid;
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

impl PortfolioGrid {
    /// Whether the grid names no group.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The index of the group `market` is in, if any.
    pub fn group_of(&self, market: MarketId) -> Option<usize> {
        self.groups.iter().position(|group| group.members.iter().any(|member| member.market == market))
    }

    /// Each group's floor share, by name.
    pub fn floor_shares(&self) -> BTreeMap<String, i64> {
        self.groups.iter().map(|group| (group.name.clone(), group.floor_share_bps)).collect()
    }

    /// Sets each named group's floor share to `shares`' value for it.
    pub(crate) fn set_floor_shares(&mut self, shares: &BTreeMap<String, i64>) {
        for group in &mut self.groups {
            if let Some(share) = shares.get(&group.name) {
                group.floor_share_bps = *share;
            }
        }
    }

    /// Checks the grid moves from `launch` only as D11 allows: the same groups, in order, with the same
    /// name, members, loadings and bounds, and each residual shock and floor share from its launch value
    /// up to its bound.
    /// Error cases:
    /// - a group or member added, removed, renamed, reordered or moved
    /// - a changed loading or bound
    /// - a residual shock or floor share below its launch value or above its bound
    pub fn check_within_launch(&self, launch: &PortfolioGrid) -> Result<(), String> {
        if self.groups.len() != launch.groups.len() {
            return Err("the grid's groups are fixed at its launch grid".into());
        }
        for (group, launched) in self.groups.iter().zip(&launch.groups) {
            let name = &launched.name;
            let same_members = group.members.len() == launched.members.len()
                && group.members.iter().zip(&launched.members).all(|(member, at)| {
                    member.market == at.market
                        && member.loading == at.loading
                        && member.max_residual_shock_bps == at.max_residual_shock_bps
                });
            if group.name != *name || group.max_floor_share_bps != launched.max_floor_share_bps || !same_members {
                return Err(format!("group {name}'s members, loadings and bounds are fixed at its launch grid"));
            }
            within(
                &format!("group {name}'s floor share"),
                group.floor_share_bps,
                launched.floor_share_bps,
                launched.max_floor_share_bps,
            )?;
            for (member, at) in group.members.iter().zip(&launched.members) {
                let market = at.market;
                within(
                    &format!("market {market}'s residual shock"),
                    member.residual_shock_bps,
                    at.residual_shock_bps,
                    at.max_residual_shock_bps,
                )?;
            }
        }
        Ok(())
    }
}

impl Engine {
    /// The grid close-outs read: the parameters' grid from `V81`, while it names a group. With none, every
    /// rule is the one before `V81`.
    pub fn portfolio_rules(&self) -> Option<&PortfolioGrid> {
        let params = self.params.as_ref().filter(|_| self.protocol_version >= ProtocolVersion::V81)?;
        (!params.portfolio_grid.is_empty()).then_some(&params.portfolio_grid)
    }

    /// The grid new risk and withdrawals read: from `V84`, the one close-outs read (#913, D4). With none, the
    /// initial requirement is the one before `V84`.
    pub(crate) fn initial_portfolio_rules(&self) -> Option<&PortfolioGrid> {
        self.portfolio_rules().filter(|_| self.protocol_version >= ProtocolVersion::V84)
    }

    /// Checks `grid` fits the markets listed now (`PortfolioGrid::check_listings`).
    /// Error cases:
    /// - as `PortfolioGrid::check_listings`
    pub(crate) fn check_grid_listings(&self, grid: &PortfolioGrid) -> Result<(), String> {
        grid.check_listings(|id| self.markets.get(&id).map(|market| &market.spec))
    }

    /// From `V81`, records the grid in force as the launch grid with the first parameters set under it;
    /// its activation over parameters already set records it there (`start_portfolio_margin`).
    pub(crate) fn record_launch_grid(&mut self) {
        if let (Some(params), None) = (&self.params, &self.operator_bounds.launch_grid)
            && self.protocol_version >= ProtocolVersion::V81
        {
            self.operator_bounds.launch_grid = Some(params.portfolio_grid.clone());
        }
    }

    /// Checks the grid a notice names, if any: its structure, and that it moves from the launch grid only
    /// as D11 allows.
    /// Error cases:
    /// - before `V81`, or with no launch grid recorded
    /// - a grid `PortfolioGrid::validate` or `PortfolioGrid::check_within_launch` refuses
    pub(crate) fn check_grid_notice(&self, grid: Option<&PortfolioGrid>) -> Result<(), String> {
        let Some(grid) = grid else { return Ok(()) };
        if self.protocol_version < ProtocolVersion::V81 {
            return Err(format!("a portfolio grid notice needs protocol {:?}", ProtocolVersion::V81));
        }
        let launch = self.operator_bounds.launch_grid.as_ref().ok_or("no launch grid is recorded")?;
        grid.validate()?;
        grid.check_within_launch(launch)
    }

    /// From `V81`, checks parameters change the grid only within its launch bounds, to the grid a notice
    /// names whose time has come. Before `V81` a grid decides nothing, so it changes freely, for the
    /// activation to take up. A `voted` change's notice is its vote's.
    /// Error cases:
    /// - from `V81`, a changed grid `PortfolioGrid::check_within_launch` refuses
    /// - from `V81`, unless `voted`, a changed grid no notice names, or whose notice takes effect later
    ///   than the sequenced time or from `V88` has lapsed
    pub(crate) fn check_grid_change(
        &self,
        grid: &PortfolioGrid,
        current: &PortfolioGrid,
        voted: bool,
    ) -> Result<(), String> {
        if grid == current || self.protocol_version < ProtocolVersion::V81 {
            return Ok(());
        }
        let launch = self.operator_bounds.launch_grid.as_ref().expect("recorded with the first parameters under V81");
        grid.check_within_launch(launch)?;
        if voted {
            return Ok(());
        }
        match self.operator_bounds.notice.as_ref().filter(|notice| notice.portfolio_grid.as_ref() == Some(grid)) {
            None => Err("a portfolio grid change needs a notice of at least seven days".into()),
            Some(notice) => self.check_notice_in_force(notice.effective_at, "the portfolio grid change"),
        }
    }

    /// Starts portfolio margin when `version` activates `V81` over the log: the grid the parameters name
    /// becomes the launch grid.
    pub(crate) fn start_portfolio_margin(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V81 || version < ProtocolVersion::V81 {
            return;
        }
        let Some(params) = &self.params else { return };
        self.operator_bounds.launch_grid = Some(params.portfolio_grid.clone());
    }
}

/// Spends a notice's grid once the parameters that apply it take effect.
pub(crate) fn spend_grid_notice(notice: &mut ParamsNotice, before: &PortfolioGrid, applied: &PortfolioGrid) {
    if applied != before && notice.portfolio_grid.as_ref() == Some(applied) {
        notice.portfolio_grid = None;
    }
}

/// Checks `value`, named `what`, is from `launch` to `bound`.
/// Error cases:
/// - `value` below `launch` or above `bound`
fn within(what: &str, value: i64, launch: i64, bound: i64) -> Result<(), String> {
    if !(launch..=bound).contains(&value) {
        return Err(format!("{what} {value} must be from its launch value {launch} to its bound {bound}"));
    }
    Ok(())
}
