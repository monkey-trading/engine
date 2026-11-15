//! Earned leverage, from `V64`: a volume tier's margin floor raises each margin band of the account's
//! initial requirement to at least the floor (`Engine::equity_and_initial_margin`), so a fresh wallet
//! opens at most 20x and the top tiers reach the market's own bands. The initial requirement admits
//! fills that add risk and withdrawals; close-outs read the market's bands alone, so a falling tier
//! closes nothing out. The floors move only with the table (`bounds`), never below launch's.

use crate::engine::Engine;
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;
use crate::volume_tier::VolumeTier;

/// The margin floors `V64`'s activation gives a table, by tier: 5% at tier 0 (20x) and 2% at tier 1
/// (50x); the tiers above take none, so they trade at the market's own bands.
pub const ACTIVATION_MARGIN_FLOORS_BPS: [i64; 2] = [500, 200];

impl Engine {
    /// The margin floor `tier`, a tier of the table in force, sets on an initial requirement: from
    /// `V64` its own, and zero before it or with no table.
    pub(crate) fn margin_floor_of(&self, tier: Option<&VolumeTier>) -> i64 {
        match tier {
            Some(tier) if self.protocol_version >= ProtocolVersion::V64 => tier.margin_floor_bps,
            _ => 0,
        }
    }

    /// The margin floor `account`'s tier sets on its initial requirement now (`margin_floor_of`).
    pub(crate) fn margin_floor_bps(&self, account: AccountId) -> i64 {
        self.margin_floor_of(self.tier_in_force(account))
    }

    /// Starts earned leverage when `version` activates `V64` over the log: each tier of the table in
    /// force, and of the launch table that bounds it, takes the larger of the floor it names, which
    /// charged nothing before, and its floor by position (`ACTIVATION_MARGIN_FLOORS_BPS`).
    pub(crate) fn start_margin_floors(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V64 || version < ProtocolVersion::V64 {
            return;
        }
        if let Some(params) = self.params.as_mut() {
            give_activation_floors(&mut params.volume_tiers);
        }
        if let Some(launch) = self.operator_bounds.launch_volume_tiers.as_mut() {
            give_activation_floors(launch);
        }
    }
}

/// Raises each tier of `tiers` to at least its floor by position (`ACTIVATION_MARGIN_FLOORS_BPS`); the
/// tiers past them keep theirs. Both are non-increasing by tier, so the result is too.
fn give_activation_floors(tiers: &mut [VolumeTier]) {
    for (tier, floor) in tiers.iter_mut().zip(ACTIVATION_MARGIN_FLOORS_BPS) {
        tier.margin_floor_bps = tier.margin_floor_bps.max(floor);
    }
}
