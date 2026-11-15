//! Request and order limits, from `V66`: a volume tier's resting-order limit caps the orders an account
//! rests at once, in place of the parameters' cap, and its request rate is what the node admits the
//! account's signed requests at (`Engine::request_limit`). A fresh wallet starts at tier 0's, so
//! recycling wallets forfeits the higher tiers' limits. A limit never cancels: orders already resting
//! when the tier falls stay, and only a new resting order past the limit is refused. A tier that names
//! no limit leaves the parameters' cap and the node's configured rate. The limits move only with the
//! table (`bounds`), never above launch's.

use crate::engine::Engine;
use crate::params::MAX_RESTING_ORDERS;
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;
use crate::volume_tier::VolumeTier;

/// The resting-order limits `V66`'s activation gives a table that names none, by tier.
pub const ACTIVATION_MAX_RESTING_ORDERS: [usize; 4] = [25, 50, 100, 250];
/// The request rates, a second, `V66`'s activation gives a table that names none, by tier.
pub const ACTIVATION_REQUESTS_PER_SECOND: [u32; 4] = [2, 5, 10, 20];
/// The highest request rate a tier may name.
pub const MAX_REQUESTS_PER_SECOND: u32 = 1_000;

/// The request rate of an account's tier, for the node's per-account and session-key buckets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestLimit {
    /// The account's volume tier.
    pub tier: usize,
    /// The signed requests a second its tier admits.
    pub per_second: u32,
}

/// Whether a limit is unnamed, which the encoding leaves out.
pub(super) fn is_unnamed<T: Default + PartialEq>(limit: &T) -> bool {
    *limit == T::default()
}

/// Whether any tier names a resting-order limit or a request rate.
pub(crate) fn names_limits(tiers: &[VolumeTier]) -> bool {
    tiers.iter().any(|tier| tier.max_resting_orders != 0 || tier.requests_per_second != 0)
}

/// Checks the tiers' resting-order limits and request rates are each named for every tier or none,
/// within their maximum, and never fall with the tier.
/// Error cases:
/// - a limit named for some tiers and not others, above its maximum, or below the tier before it
pub(super) fn validate_limits(tiers: &[VolumeTier]) -> Result<(), String> {
    let orders: Vec<usize> = tiers.iter().map(|tier| tier.max_resting_orders).collect();
    if !named_in_order(&orders, MAX_RESTING_ORDERS) {
        return Err(format!(
            "max_resting_orders is named for every tier or none, in 1..={MAX_RESTING_ORDERS}, and never falls with the tier"
        ));
    }
    let rates: Vec<u32> = tiers.iter().map(|tier| tier.requests_per_second).collect();
    if !named_in_order(&rates, MAX_REQUESTS_PER_SECOND) {
        return Err(format!(
            "requests_per_second is named for every tier or none, in 1..={MAX_REQUESTS_PER_SECOND}, and never falls with the tier"
        ));
    }
    Ok(())
}

/// Whether `values` are all zero, or all in `1..=max` and never falling.
fn named_in_order<T: Copy + Ord + Default>(values: &[T], max: T) -> bool {
    let unnamed = T::default();
    if values.first().is_none_or(|first| *first == unnamed) {
        return values.iter().all(|value| *value == unnamed);
    }
    values.iter().all(|value| *value <= max) && values.windows(2).all(|pair| pair[0] <= pair[1])
}

/// Checks `tiers` names no limit above the one `launch` names for the same tier, and names one wherever
/// `launch` does.
/// Error cases:
/// - a limit or rate above its launch value, or unnamed where launch names it
pub(super) fn within_launch_limits(tiers: &[VolumeTier], launch: &[VolumeTier]) -> Result<(), String> {
    for (at, (tier, launched)) in tiers.iter().zip(launch).enumerate() {
        let orders = (tier.max_resting_orders, launched.max_resting_orders);
        if orders.1 != 0 && !(1..=orders.1).contains(&orders.0) {
            return Err(format!(
                "volume tier {at}'s max_resting_orders {} must be named and at most its launch value {}",
                orders.0, orders.1
            ));
        }
        let rates = (tier.requests_per_second, launched.requests_per_second);
        if rates.1 != 0 && !(1..=rates.1).contains(&rates.0) {
            return Err(format!(
                "volume tier {at}'s requests_per_second {} must be named and at most its launch value {}",
                rates.0, rates.1
            ));
        }
    }
    Ok(())
}

impl Engine {
    /// The resting orders and request rate tier `tier` of the table in force names: from `V66`, each
    /// where it is not zero. `None` for each before `V66`, with no table, or where the tier names none.
    pub(super) fn tier_limits(&self, tier: usize) -> (Option<usize>, Option<u32>) {
        let named = self.volume_tiers_in_force().filter(|_| self.protocol_version >= ProtocolVersion::V66);
        let Some(tier) = named.map(|tiers| &tiers[tier]) else { return (None, None) };
        (
            Some(tier.max_resting_orders).filter(|max| *max != 0),
            Some(tier.requests_per_second).filter(|rate| *rate != 0),
        )
    }

    /// The tier `account` trades at and the resting orders it may hold at once, where its tier names a
    /// limit (`tier_limits`); otherwise the parameters' cap applies.
    pub(crate) fn order_limit(&self, account: AccountId) -> Option<(usize, usize)> {
        let tier = self.volume_tier(account);
        self.tier_limits(tier).0.map(|max| (tier, max))
    }

    /// The request rate `account`'s tier admits, which the node's buckets for the account and its
    /// session keys take, where its tier names one (`tier_limits`); otherwise the node's configured
    /// rate applies.
    pub fn request_limit(&self, account: AccountId) -> Option<RequestLimit> {
        let tier = self.volume_tier(account);
        self.tier_limits(tier).1.map(|per_second| RequestLimit { tier, per_second })
    }

    /// Starts tier limits when `version` activates `V66` over the log: each tier of the table in force,
    /// and of the launch table that bounds it, that names no limit takes its position's
    /// (`ACTIVATION_MAX_RESTING_ORDERS`, `ACTIVATION_REQUESTS_PER_SECOND`), and keeps one it names. Orders
    /// already resting stay.
    pub(crate) fn start_tier_limits(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V66 || version < ProtocolVersion::V66 {
            return;
        }
        if let Some(params) = self.params.as_mut() {
            give_activation_limits(&mut params.volume_tiers);
        }
        if let Some(launch) = self.operator_bounds.launch_volume_tiers.as_mut() {
            give_activation_limits(launch);
        }
    }
}

/// Gives each tier of `tiers` that names no limit its position's; a tier past the last position takes
/// the last's. A table names its limits for every tier or none, so the result never falls with the tier.
fn give_activation_limits(tiers: &mut [VolumeTier]) {
    let (orders, rates) = (&ACTIVATION_MAX_RESTING_ORDERS, &ACTIVATION_REQUESTS_PER_SECOND);
    for (at, tier) in tiers.iter_mut().enumerate() {
        if tier.max_resting_orders == 0 {
            tier.max_resting_orders = orders[at.min(orders.len() - 1)];
        }
        if tier.requests_per_second == 0 {
            tier.requests_per_second = rates[at.min(rates.len() - 1)];
        }
    }
}
