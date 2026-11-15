//! Tier benefits, from `V68`: a volume tier's fee discount, financing discount and emissions boost.
//!
//! - **Fee discount.** The base the profit fee's hold curve is compared with falls by the discount,
//!   `fee = max(base x (1 - d), C(hold))`, rounded up to the bp (`holding_fee`), so a hold the curve
//!   prices above every tier's discounted base pays the curve whatever the tier. The tier is read at the
//!   close.
//! - **Financing discount.** At each rollover a position's markup falls by its account's discount, so a
//!   long pays the benchmark plus the discounted markup and a short receives the benchmark less it
//!   (`funds`). A negative markup is left as listed, since a discount never makes an account pay more.
//!   The tier is read at the scheduled rollover, however late a command catches it up.
//! - **Emissions boost.** An account's weight in a period, once the emission debt has taken its part
//!   (`emissions`), grows by its tier's boost before the period's budget is shared; the boost is the
//!   share added, so 2,500 bps weighs 1.25 times. The tier is read at the period's end, however late a
//!   command closes it.
//! - **Bounds.** The benefits move only with the table (`bounds`): each at or below launch's, so none
//!   is more generous than at launch, and each never falls with the tier. A lower tier's discount is
//!   never above a higher tier's, so lowering a higher tier's cannot make it pay more.

use crate::engine::Engine;
use crate::params::MAX_RATE_BPS;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, Millis};
use crate::volume_tier::VolumeTier;

/// The fee discounts, in bps of the base, `V68`'s activation gives a table that names none, by tier:
/// 0, 0, 10% and 20%, so a 10% base is 10%, 10%, 9% and 8%.
pub const ACTIVATION_FEE_DISCOUNTS_BPS: [i64; 4] = [0, 0, 1_000, 2_000];
/// The financing discounts, in bps of the markup, `V68`'s activation gives a table that names none.
pub const ACTIVATION_FINANCING_DISCOUNTS_BPS: [i64; 4] = [0, 0, 1_000, 2_000];
/// The emissions boosts, in bps added to the weight, `V68`'s activation gives a table that names none:
/// 1x, 1.1x, 1.25x and 1.5x.
pub const ACTIVATION_EMISSIONS_BOOSTS_BPS: [i64; 4] = [0, 1_000, 2_500, 5_000];
/// The largest boost a tier may name: ten times the weight added.
const MAX_BOOST_BPS: i64 = 100_000;

/// Whether any tier names a fee discount, a financing discount or an emissions boost.
pub(crate) fn names_benefits(tiers: &[VolumeTier]) -> bool {
    tiers
        .iter()
        .any(|tier| tier.fee_discount_bps != 0 || tier.financing_discount_bps != 0 || tier.emissions_boost_bps != 0)
}

/// Checks the tiers' fee discounts, financing discounts and emissions boosts are within their maxima
/// and never fall with the tier.
/// Error cases:
/// - a discount outside 0..=10000 bps or a boost outside 0..=100000 bps
/// - a discount or boost below that of the tier before it
pub(super) fn validate_benefits(tiers: &[VolumeTier]) -> Result<(), String> {
    let column = |read: fn(&VolumeTier) -> i64| tiers.iter().map(read).collect::<Vec<i64>>();
    let columns = [
        ("fee_discount_bps", MAX_RATE_BPS, column(|tier| tier.fee_discount_bps)),
        ("financing_discount_bps", MAX_RATE_BPS, column(|tier| tier.financing_discount_bps)),
        ("emissions_boost_bps", MAX_BOOST_BPS, column(|tier| tier.emissions_boost_bps)),
    ];
    for (name, max, values) in columns {
        if values.iter().any(|bps| !(0..=max).contains(bps)) || values.windows(2).any(|pair| pair[0] > pair[1]) {
            return Err(format!("{name} must be in 0..={max} and never fall with the tier"));
        }
    }
    Ok(())
}

/// Checks `tiers` names no benefit above the one `launch` names for the same tier.
/// Error cases:
/// - a fee discount, financing discount or emissions boost above its launch value
pub(super) fn within_launch_benefits(tiers: &[VolumeTier], launch: &[VolumeTier]) -> Result<(), String> {
    for (at, (tier, launched)) in tiers.iter().zip(launch).enumerate() {
        let named = [
            ("fee_discount_bps", tier.fee_discount_bps, launched.fee_discount_bps),
            ("financing_discount_bps", tier.financing_discount_bps, launched.financing_discount_bps),
            ("emissions_boost_bps", tier.emissions_boost_bps, launched.emissions_boost_bps),
        ];
        if let Some((name, bps, launch_bps)) = named.into_iter().find(|(_, bps, launch_bps)| bps > launch_bps) {
            return Err(format!("volume tier {at}'s {name} {bps} is above its launch value {launch_bps}"));
        }
    }
    Ok(())
}

impl Engine {
    /// The tier `account` trades at at `at`, from `V68`, where its benefits apply; `None` before it, or
    /// with no table. The version is checked first, so no earlier log pays for the tier.
    fn benefit_tier_at(&self, account: AccountId, at: Millis) -> Option<&VolumeTier> {
        if self.protocol_version < ProtocolVersion::V68 {
            return None;
        }
        self.tier_in_force_at(account, at)
    }

    /// The share of the base profit fee `account`'s tier takes off now, in bps: from `V68` its tier's at
    /// the close the command makes, and zero before it.
    pub(crate) fn fee_discount_bps(&self, account: AccountId) -> i64 {
        self.benefit_tier_at(account, self.hold_clock_time()).map_or(0, |tier| tier.fee_discount_bps)
    }

    /// The share of a position's markup `account`'s tier takes off at the rollover at `at`, in bps: from
    /// `V68` its tier's then, however late a command catches the rollover up, and zero before it.
    pub(crate) fn financing_discount_bps(&self, account: AccountId, at: Millis) -> i64 {
        self.benefit_tier_at(account, at).map_or(0, |tier| tier.financing_discount_bps)
    }

    /// The share `account`'s tier adds to its emission weight for the period that ends at `at`, in bps:
    /// from `V68` its tier's then, however late a command closes the period, and zero before it.
    pub(crate) fn emissions_boost_bps(&self, account: AccountId, at: Millis) -> i64 {
        self.benefit_tier_at(account, at).map_or(0, |tier| tier.emissions_boost_bps)
    }

    /// The fee discount, financing discount and emissions boost of tier `tier` of the table in force, in
    /// bps: each where the version has them, and `None` for each before `V68`.
    pub(super) fn tier_benefits(&self, tier: usize) -> (Option<i64>, Option<i64>, Option<i64>) {
        let named = self.volume_tiers_in_force().filter(|_| self.protocol_version >= ProtocolVersion::V68);
        let Some(tier) = named.map(|tiers| &tiers[tier]) else { return (None, None, None) };
        (Some(tier.fee_discount_bps), Some(tier.financing_discount_bps), Some(tier.emissions_boost_bps))
    }

    /// Starts tier benefits when `version` activates `V68` over the log: each tier of the table in force,
    /// and of the launch table that bounds it, takes the benefits of its position
    /// (`ACTIVATION_FEE_DISCOUNTS_BPS`, `ACTIVATION_FINANCING_DISCOUNTS_BPS`,
    /// `ACTIVATION_EMISSIONS_BOOSTS_BPS`); a table before `V68` names none.
    pub(crate) fn start_tier_benefits(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V68 || version < ProtocolVersion::V68 {
            return;
        }
        if let Some(params) = self.params.as_mut() {
            give_activation_benefits(&mut params.volume_tiers);
        }
        if let Some(launch) = self.operator_bounds.launch_volume_tiers.as_mut() {
            give_activation_benefits(launch);
        }
    }
}

/// Gives each tier of `tiers` its position's benefits; a tier past the last position takes the last's,
/// so the result never falls with the tier.
fn give_activation_benefits(tiers: &mut [VolumeTier]) {
    let last = ACTIVATION_FEE_DISCOUNTS_BPS.len() - 1;
    for (at, tier) in tiers.iter_mut().enumerate() {
        let at = at.min(last);
        tier.fee_discount_bps = ACTIVATION_FEE_DISCOUNTS_BPS[at];
        tier.financing_discount_bps = ACTIVATION_FINANCING_DISCOUNTS_BPS[at];
        tier.emissions_boost_bps = ACTIVATION_EMISSIONS_BOOSTS_BPS[at];
    }
}
