//! Portfolio margin: an account's requirement under a grid of shocks, which close-outs read from `V81`
//! while the parameters name a grid (`closeout`).
//!
//! The markets of an offset group share an asset class, one schedule and USD as quote currency, so
//! each member's profit and loss is linear in its own price, and each moves with the group's factor by
//! its loading β. A position's loading-weighted exposure is `β × qty × price`. In each group, the
//! exposure on its smaller side, long or short, offsets as much on its larger side: the matched
//! exposure is the group's gross loading-weighted exposure less the magnitude of its net, the smaller
//! side whole and that share of each position on the larger side. Its net is zero, so the factor's
//! move cancels on it and its worst loss over the grid is each member's own move of ±ρ.
//!
//! Exposure that offsets nothing keeps its per-position requirement, bands included, so a lone
//! position's requirement is its per-position one exactly. Matched exposure gets the grid's credit:
//! its share of the bands less its worst loss and the cost of closing it at its close-spread ceilings
//! over the close-out level, at most `1 - φ` of that share, φ the group's floor share, and never
//! below zero. The portfolio requirement is the per-position requirement less every group's credit,
//! so it is never above per-position margin. The grid has one shock per member, the same at every time,
//! a weekend gap included. At initial bands each band charges at least the account's tier floor and,
//! over its market's weekend window, its weekend margin, and a group takes no credit while a member the
//! account holds cannot trade.

mod bounds;
mod closeout;
mod exposure;
mod reach;
mod requirement;
pub(crate) use bounds::spend_grid_notice;
pub(crate) use closeout::CloseoutContext;
pub use reach::PortfolioMove;
pub(crate) use requirement::{Basis, Valued, at_mark};
pub use requirement::{FactorMove, GroupStanding, PortfolioStanding};

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::market::MarketSpec;
use crate::types::MarketId;

/// The most a shock or a floor share may be: 100%.
const MAX_BPS: i64 = 10_000;
/// The largest loading of either sign: a member moving ten times as far as its group's factor. It
/// bounds the exposures' range (`exposure`).
pub const MAX_LOADING: Fixed = Fixed::from_int(10);

/// The offset groups an account's portfolio requirement is computed under. Empty, no group offsets
/// anything.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortfolioGrid {
    pub groups: Vec<OffsetGroup>,
}

/// Markets whose positions offset each other through a common factor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OffsetGroup {
    /// The name the account read keys the group by.
    pub name: String,
    /// φ: the least share of its matched exposure's per-position requirement the group keeps, in bps.
    pub floor_share_bps: i64,
    /// The most a notice or a vote may raise φ to, fixed with the launch grid (`bounds`).
    pub max_floor_share_bps: i64,
    pub members: Vec<GroupMember>,
}

/// One market of an offset group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupMember {
    pub market: MarketId,
    /// β: how far the member moves with the group's factor, either sign, never zero.
    pub loading: Fixed,
    /// ρ: the member's own move either way, in bps of its notional.
    pub residual_shock_bps: i64,
    /// The most a notice or a vote may raise ρ to, fixed with the launch grid (`bounds`).
    pub max_residual_shock_bps: i64,
}

impl PortfolioGrid {
    /// Checks the grid's structure.
    /// Error cases:
    /// - a group with an empty name, a name another group has, or fewer than two members
    /// - a floor share outside 1..=10000 bps, or a bound on it below it or above 10000 bps
    /// - a market in more than one group, or twice in one
    /// - a residual shock outside 0..=10000 bps, or a bound on it below it or above 10000 bps
    /// - a loading of zero, or of a magnitude above `MAX_LOADING`
    pub fn validate(&self) -> Result<(), String> {
        let (mut names, mut markets) = (BTreeSet::new(), BTreeSet::new());
        for group in &self.groups {
            let name = &group.name;
            if name.is_empty() || !names.insert(name.as_str()) {
                return Err(format!("group names must be non-empty and distinct: {name:?}"));
            }
            if group.members.len() < 2 {
                return Err(format!("group {name} needs at least two members"));
            }
            if !(1..=MAX_BPS).contains(&group.floor_share_bps) {
                return Err(format!("group {name}: floor_share_bps must be in 1..=10000"));
            }
            if !(group.floor_share_bps..=MAX_BPS).contains(&group.max_floor_share_bps) {
                return Err(format!("group {name}: max_floor_share_bps must be from its floor share to 10000"));
            }
            for member in &group.members {
                let market = member.market;
                if !markets.insert(market) {
                    return Err(format!("market {market} is in more than one place in the grid"));
                }
                if member.loading.is_zero() || member.loading.abs() > MAX_LOADING {
                    return Err(format!(
                        "group {name}: market {market}'s loading must be non-zero and at most 10 either way"
                    ));
                }
                let shock = member.residual_shock_bps;
                if !(0..=MAX_BPS).contains(&shock) {
                    return Err(format!("group {name}: market {market}'s residual shock must be in 0..=10000 bps"));
                }
                if !(shock..=MAX_BPS).contains(&member.max_residual_shock_bps) {
                    return Err(format!(
                        "group {name}: market {market}'s bound on its residual shock must be from the shock to 10000"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Checks each group's listed members against their listings, `listing` giving a market's: one
    /// asset class, one schedule and USD as quote currency. A member not listed counts for nothing.
    /// Error cases:
    /// - a listed member quoted in a currency other than USD
    /// - two listed members of one group in different asset classes or on different schedules
    pub fn check_listings<'m>(&self, listing: impl Fn(MarketId) -> Option<&'m MarketSpec>) -> Result<(), String> {
        for group in &self.groups {
            let mut listed = group.members.iter().filter_map(|member| listing(member.market));
            let Some(first) = listed.next() else { continue };
            for spec in std::iter::once(first).chain(listed) {
                if !spec.quote_currency.is_usd() {
                    return Err(format!("group {}: market {} is not quoted in USD", group.name, spec.id));
                }
                if spec.asset_class != first.asset_class || spec.schedule != first.schedule {
                    return Err(format!(
                        "group {}: markets {} and {} differ in asset class or schedule",
                        group.name, first.id, spec.id
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
