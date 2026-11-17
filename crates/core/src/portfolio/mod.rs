//! Portfolio margin as a read: an account's requirement under a grid of shocks, which nothing the
//! engine decides reads.
//!
//! The markets of an offset group share an asset class, one schedule and USD as quote currency, so
//! each member's profit and loss is linear in its own price. Each moves with the group's factor by
//! its loading β and by a residual of its own. With N each position's signed USD notional, the worst
//! loss of a group's positions over the grid of the factor moving ±F and each residual moving ±ρ is
//! `L = F × |Σ β N| + Σ ρ |N|`. The model is `M = Σ L + the bands of every position in no group +
//! the cost of closing every position at its close-spread ceiling, over the close-out level`, and the
//! portfolio requirement is `R = min(PP, max(φ × PP, M))`, PP the per-position requirement and φ the
//! floor share: never above per-position margin, never below its share of it, and PP itself while no
//! group offsets anything. At initial bands, PP charges each band at least the account's tier floor
//! and, over a market's weekend window, its weekend margin; a group takes its weekend shocks while a
//! listed member's weekend margin applies, and counts at its members' bands while a member the
//! account holds cannot trade; and R is at least the tier floor on the sum of notionals.

mod requirement;
pub use requirement::{FactorMove, GroupStanding, PortfolioStanding};

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::market::MarketSpec;
use crate::types::MarketId;

/// The most a shock or the floor share may be: 100%.
const MAX_BPS: i64 = 10_000;
/// The largest loading of either sign: a member moving ten times as far as its group's factor. It
/// bounds the net's range (`requirement::loading_weighted_net`).
pub const MAX_LOADING: Fixed = Fixed::from_int(10);

/// The offset groups and the floor share an account's portfolio requirement is computed under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortfolioGrid {
    /// φ: the least share of the per-position requirement the portfolio requirement is, in bps.
    pub floor_share_bps: i64,
    pub groups: Vec<OffsetGroup>,
}

/// Markets whose positions offset each other through a common factor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OffsetGroup {
    /// The name the account read keys the group by.
    pub name: String,
    /// F: the factor's move either way, in bps of the group's loading-weighted net notional.
    pub factor_shock_bps: i64,
    /// F over the group's weekend window, for the initial requirement; at least `factor_shock_bps`.
    pub weekend_factor_shock_bps: i64,
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
    /// ρ over the group's weekend window, for the initial requirement; at least `residual_shock_bps`.
    pub weekend_residual_shock_bps: i64,
}

impl PortfolioGrid {
    /// Checks the grid's structure.
    /// Error cases:
    /// - a floor share outside 1..=10000 bps
    /// - a group with an empty name, a name another group has, or fewer than two members
    /// - a market in more than one group, or twice in one
    /// - a factor shock outside 1..=10000 bps, or a residual shock outside 0..=10000 bps
    /// - a weekend shock below its normal shock or above 10000 bps
    /// - a loading of zero, or of a magnitude above `MAX_LOADING`
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_BPS).contains(&self.floor_share_bps) {
            return Err("floor_share_bps must be in 1..=10000".into());
        }
        let (mut names, mut markets) = (BTreeSet::new(), BTreeSet::new());
        for group in &self.groups {
            let name = &group.name;
            if name.is_empty() || !names.insert(name.as_str()) {
                return Err(format!("group names must be non-empty and distinct: {name:?}"));
            }
            if group.members.len() < 2 {
                return Err(format!("group {name} needs at least two members"));
            }
            check_shocks(name, "factor", group.factor_shock_bps, group.weekend_factor_shock_bps, 1)?;
            for member in &group.members {
                if !markets.insert(member.market) {
                    return Err(format!("market {} is in more than one place in the grid", member.market));
                }
                if member.loading.is_zero() || member.loading.abs() > MAX_LOADING {
                    return Err(format!(
                        "group {name}: market {}'s loading must be non-zero and at most 10 either way",
                        member.market
                    ));
                }
                let (normal, weekend) = (member.residual_shock_bps, member.weekend_residual_shock_bps);
                check_shocks(name, &format!("market {}'s residual", member.market), normal, weekend, 0)?;
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

    /// The group `market` is a member of, if any.
    fn group_of(&self, market: MarketId) -> Option<&OffsetGroup> {
        self.groups.iter().find(|group| group.members.iter().any(|member| member.market == market))
    }
}

/// Checks a normal shock and its weekend value, `what` naming them.
/// Error cases:
/// - the normal shock outside `least..=10000` bps
/// - the weekend shock below the normal one or above 10000 bps
fn check_shocks(group: &str, what: &str, normal: i64, weekend: i64, least: i64) -> Result<(), String> {
    if !(least..=MAX_BPS).contains(&normal) || !(normal..=MAX_BPS).contains(&weekend) {
        return Err(format!(
            "group {group}: the {what} shock must be in {least}..=10000 bps, and its weekend shock from it to 10000"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
