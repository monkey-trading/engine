//! The operator's bounds on the tier table, from `V59`: the table moves only as a seven-day notice
//! names it, or by a vote, which is held to the same bounds. A table keeps the launch table's tiers,
//! each threshold and day divisor at or above launch's and each share at or below it, and with it,
//! in every listed market, each tier's spread bands charge at every notional from that tier's launch
//! spread, the market's launch bands at the launch share, to twice it. A listing's new spread bands
//! are held to the same check at every tier of the table in force.

use crate::engine::Engine;
use crate::listing_bounds::check_spread_within_launch;
use crate::operator_bounds::ParamsNotice;
use crate::params::ProtocolParams;
use crate::protocol::ProtocolVersion;
use crate::spread_tier::SpreadTier;
use crate::types::MarketId;
use crate::volume_tier::{AccountBands, VolumeTier, launch_volume_tiers, validate_volume_tiers};

impl Engine {
    /// Checks an activation carrying the log to `V59` or later from before it takes up a table no more
    /// generous than `launch_volume_tiers` (`within_launch_table`), so parameters set before `V59` cannot
    /// make a launch table that later bounds measure from, and leaves every listed market within the
    /// tiered bound at that table: the parameters' table, or `launch_volume_tiers` for parameters that
    /// name none. A listing a notice moved before `V59` can move a band bound so a tier's first band ends
    /// past where it did at launch, which the activation would take up as charging more than twice that
    /// tier's launch spread.
    /// Error cases:
    /// - parameters naming a table more generous than `launch_volume_tiers`, or of another shape
    /// - a listed market whose bands at some tier of that table charge outside its launch spread to twice
    ///   it
    pub(crate) fn check_volume_tier_activation(&self, version: ProtocolVersion) -> Result<(), String> {
        let Some(params) = self.params.as_ref().filter(|_| self.protocol_version < ProtocolVersion::V59) else {
            return Ok(());
        };
        if version < ProtocolVersion::V59 {
            return Ok(());
        }
        let mut tiered = params.clone();
        tiered.convert_to_tiered();
        let table = &tiered.volume_tiers;
        within_launch_table(table, &launch_volume_tiers())?;
        self.markets
            .iter()
            .try_for_each(|(id, market)| self.check_tiered_against(*id, &market.spec.spread_tiers, table, table))
    }

    /// Starts volume tiers when `version` activates `V59` over the log: parameters that name no table
    /// take `launch_volume_tiers`, and the table in force becomes the launch table. No account has
    /// counted volume yet, so every account is at tier 0, and every held position is pending for its
    /// first `MIN_HOLD_MS` (`Engine::hold_held_positions`).
    pub(crate) fn start_volume_tiers(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V59 || version < ProtocolVersion::V59 {
            return;
        }
        let Some(params) = self.params.as_mut() else { return };
        params.convert_to_tiered();
        self.operator_bounds.launch_volume_tiers = Some(params.volume_tiers.clone());
        self.hold_held_positions();
    }

    /// From `V59`, records the table in force as the launch table with the first parameters set under
    /// it; its activation over parameters already set records it there (`start_volume_tiers`).
    pub(crate) fn record_launch_volume_tiers(&mut self) {
        if let (Some(params), None) = (&self.params, &self.operator_bounds.launch_volume_tiers)
            && self.protocol_version >= ProtocolVersion::V59
        {
            self.operator_bounds.launch_volume_tiers = Some(params.volume_tiers.clone());
        }
    }

    /// Checks the table a notice names, if any: its shape (`validate_volume_tiers`) and its bound
    /// (`check_volume_tier_bound`).
    /// Error cases:
    /// - before `V59`
    /// - a table `validate_volume_tiers` or `check_volume_tier_bound` refuses
    pub(crate) fn check_volume_tier_notice(&self, tiers: Option<&[VolumeTier]>) -> Result<(), String> {
        let Some(tiers) = tiers else { return Ok(()) };
        if self.protocol_version < ProtocolVersion::V59 {
            return Err(format!("a volume tier table needs protocol {:?}", ProtocolVersion::V59));
        }
        validate_volume_tiers(tiers)?;
        self.check_volume_tier_bound(tiers)
    }

    /// From `V59`, checks parameters change the table only to one within its bound
    /// (`check_volume_tier_bound`), named by a notice whose time has come. Before `V59` a table prices
    /// nothing, and its activation holds the table to `launch_volume_tiers`
    /// (`Engine::check_volume_tier_activation`). A `voted` change's notice is its vote's.
    /// Error cases:
    /// - from `V59`, a changed table `check_volume_tier_bound` refuses
    /// - from `V59`, unless `voted`, a changed table no notice names, or whose notice takes effect later
    ///   than the sequenced time
    pub(crate) fn check_volume_tier_change(
        &self,
        params: &ProtocolParams,
        current: &ProtocolParams,
        voted: bool,
    ) -> Result<(), String> {
        let tiers = &params.volume_tiers;
        if *tiers == current.volume_tiers || self.protocol_version < ProtocolVersion::V59 {
            return Ok(());
        }
        self.check_volume_tier_bound(tiers)?;
        if voted {
            return Ok(());
        }
        let named = self.operator_bounds.notice.as_ref().filter(|notice| notice.volume_tiers.as_ref() == Some(tiers));
        match named {
            None => Err("a volume tier table change needs a notice of at least seven days".into()),
            Some(notice) if notice.effective_at > self.now => {
                Err(format!("the volume tier table change takes effect from {}", notice.effective_at))
            }
            Some(_) => Ok(()),
        }
    }

    /// Checks `tiers` against the launch table and the listings in force: as many tiers as at launch,
    /// each threshold and day divisor at or above its launch value and each share at or below it, and
    /// every listed market's bands within their launch spread at every tier (`check_tiered_spreads`),
    /// and, at a tier needing more than at launch, within it at every tier below.
    /// Error cases:
    /// - another number of tiers than at launch
    /// - a threshold or day divisor below its launch value, or a share above it
    /// - a listed market whose bands at some tier charge outside that tier's launch spread to twice it
    /// - a raised tier whose launch spread a tier below it charges more than twice, at some notional
    fn check_volume_tier_bound(&self, tiers: &[VolumeTier]) -> Result<(), String> {
        // Recorded with the first parameters under `V59`, or at its activation over them.
        let launch = self
            .operator_bounds
            .launch_volume_tiers
            .as_deref()
            .expect("the launch volume tiers are recorded with parameters");
        within_launch_table(tiers, launch)?;
        self.markets
            .iter()
            .try_for_each(|(id, market)| self.check_tiered_spreads(*id, &market.spec.spread_tiers, tiers))
    }

    /// From `V59`, checks `spread_tiers`, a listing's spread bands for `market`, at every tier of the
    /// table in force (`check_tiered_spreads`). Before `V59`, or for a market with no launch bands yet,
    /// there is nothing to check.
    /// Error cases:
    /// - bands that at some tier charge outside that tier's launch spread to twice it
    pub(crate) fn check_tiered_listing(&self, market: MarketId, spread_tiers: &[SpreadTier]) -> Result<(), String> {
        match self.volume_tiers_in_force() {
            Some(tiers) => self.check_tiered_spreads(market, spread_tiers, tiers),
            None => Ok(()),
        }
    }

    /// Checks that at every tier of `tiers`, at every notional, `spread_tiers` with that tier's share
    /// applied charge from `market`'s launch bands at the tier's launch share to twice them: D7's
    /// protected baseline, the curve each account pays. A market with no launch bands yet, being
    /// listed for the first time, sets them.
    /// Error cases:
    /// - a tier at which the bands charge below its launch spread, or above twice it, at some notional
    fn check_tiered_spreads(
        &self,
        market: MarketId,
        spread_tiers: &[SpreadTier],
        tiers: &[VolumeTier],
    ) -> Result<(), String> {
        match self.operator_bounds.launch_volume_tiers.as_deref() {
            Some(launch) => self.check_tiered_against(market, spread_tiers, tiers, launch),
            None => Ok(()),
        }
    }

    /// `check_tiered_spreads` against the launch table `launch`.
    /// Error cases:
    /// - a tier at which the bands charge below its launch spread, or above twice it, at some notional
    fn check_tiered_against(
        &self,
        market: MarketId,
        spread_tiers: &[SpreadTier],
        tiers: &[VolumeTier],
        launch: &[VolumeTier],
    ) -> Result<(), String> {
        let Some(bands) = self.operator_bounds.launch_bands.get(&market) else { return Ok(()) };
        tiered_within(spread_tiers, &bands.spread_tiers, tiers, launch)
            .map_err(|reason| format!("market {market} at volume tier {reason}"))
    }

    /// From `V59`, checks a market's first listing, whose bands become its launch bands, at every tier
    /// of the table in force, as `check_tiered_spreads` does with those bands as the launch bands: a
    /// share cut since launch cannot list a market outside a tier's protected spread from the start.
    /// Error cases:
    /// - a tier at which the bands charge more than twice what they do at its launch share, at some
    ///   notional
    pub(crate) fn check_tiered_first_listing(
        &self,
        market: MarketId,
        spread_tiers: &[SpreadTier],
    ) -> Result<(), String> {
        let (Some(tiers), Some(launch)) =
            (self.volume_tiers_in_force(), self.operator_bounds.launch_volume_tiers.as_deref())
        else {
            return Ok(());
        };
        tiered_within(spread_tiers, spread_tiers, tiers, launch)
            .map_err(|reason| format!("market {market}'s first listing at volume tier {reason}"))
    }
}

impl ProtocolParams {
    /// Gives parameters that name no volume tier table the launch table: how `V59`'s activation
    /// converts parameters, and the form a config under `V59` gives earlier ones.
    pub fn convert_to_tiered(&mut self) {
        if self.volume_tiers.is_empty() {
            self.volume_tiers = launch_volume_tiers();
        }
    }
}

/// Checks `tiers` keeps `launch`'s number of tiers, each threshold and day divisor at or above its
/// launch value and each share at or below it.
/// Error cases:
/// - another number of tiers, a threshold or day divisor below launch's, or a share above it
fn within_launch_table(tiers: &[VolumeTier], launch: &[VolumeTier]) -> Result<(), String> {
    if tiers.len() != launch.len() {
        return Err(format!("a volume tier table keeps its {} launch tiers", launch.len()));
    }
    for (at, (tier, launched)) in tiers.iter().zip(launch).enumerate() {
        if tier.threshold < launched.threshold || tier.day_divisor < launched.day_divisor {
            return Err(format!("volume tier {at}'s threshold and day_divisor stay at or above launch's"));
        }
        if tier.best_price_share_bps > launched.best_price_share_bps {
            return Err(format!(
                "volume tier {at}'s best_price_share_bps {} is above its launch value {}",
                tier.best_price_share_bps, launched.best_price_share_bps
            ));
        }
    }
    Ok(())
}

/// Checks that an account charges at every notional from what its launch tier charged at launch to
/// twice it: at every tier of `tiers`, `spread_tiers` at that tier's share from `launch_bands` at the
/// tier's share in `launch` to twice them. A tier that needs more than at launch can drop an account
/// that reached it at launch to any tier below, so every lower tier's share is also held against it.
/// Returns the first tier outside it with the reason, as `"<tier>: <reason>"`, naming the launch tier
/// when it is another.
fn tiered_within(
    spread_tiers: &[SpreadTier],
    launch_bands: &[SpreadTier],
    tiers: &[VolumeTier],
    launch: &[VolumeTier],
) -> Result<(), String> {
    for (reached, (now, launched)) in tiers.iter().zip(launch).enumerate() {
        let protected = AccountBands::new(launch_bands, launched.best_price_share_bps).tiers();
        let raised = now.threshold > launched.threshold || now.day_divisor > launched.day_divisor;
        let lowest = if raised { 0 } else { reached };
        for (at, tier) in tiers.iter().enumerate().take(reached + 1).skip(lowest) {
            let charged = AccountBands::new(spread_tiers, tier.best_price_share_bps).tiers();
            check_spread_within_launch(&charged, &protected).map_err(|reason| {
                if at == reached {
                    format!("{at}: {reason}")
                } else {
                    format!("{at}, for an account at tier {reached} at launch: {reason}")
                }
            })?;
        }
    }
    Ok(())
}

/// Spends `notice`'s table once parameters move the table from `before` to the `applied` one it names.
pub(crate) fn spend_volume_tier_notice(notice: &mut ParamsNotice, before: &[VolumeTier], applied: &[VolumeTier]) {
    if applied != before && notice.volume_tiers.as_deref() == Some(applied) {
        notice.volume_tiers = None;
    }
}
