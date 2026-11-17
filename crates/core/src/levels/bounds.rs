//! The minimum level distance's rules, from `V79`. Every listing names a distance. A listed market's
//! moves only as a listing notice of at least seven days names it, or by a passed normal vote, from its
//! launch value to twice it (`listing_bounds`); it is no fast-path restriction, since a restriction
//! changes only the fields `governance::restriction` names. The launch value is the distance in force
//! at activation, or at a market's first listing after it. Levels already set stay when it moves.

use crate::engine::Engine;
use crate::market::MarketSpec;
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

impl Engine {
    /// On an activation carrying the log to `V79` from before it: gives each listed market listed
    /// without a minimum level distance, and each listing a ceiling adoption binds, its close-spread
    /// ceiling, and takes each listed market's distance as its launch value.
    pub(crate) fn start_levels(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V79 || version < ProtocolVersion::V79 {
            return;
        }
        let ids: Vec<MarketId> = self.markets.keys().copied().collect();
        for id in ids {
            let ceiling = self.close_spread_ceiling(id);
            let spec = &mut self.markets.get_mut(&id).expect("listed").spec;
            if spec.min_level_distance_bps.is_none() {
                spec.min_level_distance_bps = ceiling;
            }
            let distance = spec.min_level_distance_bps;
            if let Some(launch) = self.operator_bounds.launch_bands.get_mut(&id) {
                launch.min_level_distance_bps = distance;
            }
        }
        let bound: Vec<MarketId> = self.operator_bounds.proposed_listings.keys().copied().collect();
        for id in bound {
            let ceiling = self.close_spread_ceiling(id);
            let spec = self.operator_bounds.proposed_listings.get_mut(&id).expect("selected above");
            spec.min_level_distance_bps = spec.min_level_distance_bps.or(ceiling);
        }
        let listed = |id: MarketId| self.markets.contains_key(&id);
        if let Some(pending) = self.pending_ceiling.as_mut().filter(|pending| !listed(pending.market))
            && let Some(listing) = pending.listing.as_mut()
        {
            listing.min_level_distance_bps = listing.min_level_distance_bps.or(Some(pending.max_close_spread_bps));
        }
    }

    /// From `V79`, checks a listing names a minimum level distance. A listing before `V79` may name one
    /// for its activation to take up.
    /// Error cases:
    /// - from `V79`, a listing with no distance
    pub(crate) fn check_level_distance_listing(&self, spec: &MarketSpec) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V79 || spec.min_level_distance_bps.is_some() {
            return Ok(());
        }
        Err(format!("market {} names no minimum level distance", spec.id))
    }
}

/// Checks a minimum level distance a notice or a vote sets lies from the market's launch value to twice
/// it.
/// Error cases:
/// - a distance outside the launch value to twice it, or a market with no launch value
pub fn check_level_distance_range(bps: i64, launch: Option<i64>) -> Result<(), String> {
    let launch = launch.ok_or("the market has no launch minimum level distance")?;
    if !(launch..=launch.saturating_mul(2)).contains(&bps) {
        return Err(format!(
            "a minimum level distance of {bps} bps is outside {launch}..={}, its launch value to twice it",
            launch.saturating_mul(2)
        ));
    }
    Ok(())
}
