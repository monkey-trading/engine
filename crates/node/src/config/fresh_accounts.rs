//! `[api.fresh_accounts]`: what wallets that have paid nothing may add to the public log and to
//! permanent state, apart from the rate limits on trading.

use std::num::NonZeroU32;

use anyhow::{bail, ensure};
use serde::Deserialize;

use crate::config::ApiSection;

/// The most a production build lets the budgets be set to: one setup request a second for all
/// clients, with a burst of 120, and 12 a minute with a burst of 6 for one client. A setup takes two
/// requests, so these allow 86,520 log commands a day for all clients, and 17,286 for one.
const PRODUCTION_SHARED: Budget =
    Budget { per_minute: NonZeroU32::new(60).unwrap(), burst: NonZeroU32::new(120).unwrap() };
const PRODUCTION_PER_CLIENT: Budget =
    Budget { per_minute: NonZeroU32::new(12).unwrap(), burst: NonZeroU32::new(6).unwrap() };

/// A token bucket for requests slow enough to count by the minute: `burst` at once, refilled at
/// `per_minute`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub per_minute: NonZeroU32,
    pub burst: NonZeroU32,
}

/// The budgets for a never-credited account's declaration and session-key requests, each taking one
/// token from both.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreshAccountBudgets {
    /// Shared by all clients, so fresh signing keys cannot multiply it; also the rate of the budget
    /// all paid declarations share.
    pub shared: Budget,
    /// One client's share of it, so no one client takes it all. A wallet with a pending deposit has
    /// paid, so its declaration takes none from `shared` or from its client's share, and is held to
    /// this rate for the account itself and for its client's paid wallets together, however often the
    /// engine refuses it.
    pub per_client: Budget,
}

impl ApiSection {
    /// Checks one client's share is below the budget all clients share, so no one client can take
    /// all of it.
    /// Error cases:
    /// - the share's rate or burst is not below the shared budget's
    pub(super) fn validate_fresh_accounts(&self) -> anyhow::Result<()> {
        let FreshAccountBudgets { shared, per_client } = self.fresh_accounts;
        ensure!(
            per_client.per_minute < shared.per_minute && per_client.burst < shared.burst,
            "api.fresh_accounts.per_client must be below api.fresh_accounts.shared in both per_minute and burst"
        );
        Ok(())
    }

    /// Refuses budgets above the production values when `production` is set, which the dev configs'
    /// rates are.
    /// Error cases:
    /// - `production` is set and a budget's rate or burst is above the production value
    pub(super) fn refuse_dev_fresh_budgets(&self, production: bool) -> anyhow::Result<()> {
        let FreshAccountBudgets { shared, per_client } = self.fresh_accounts;
        for (key, budget, ceiling) in
            [("shared", shared, PRODUCTION_SHARED), ("per_client", per_client, PRODUCTION_PER_CLIENT)]
        {
            if production && (budget.per_minute > ceiling.per_minute || budget.burst > ceiling.burst) {
                bail!(
                    "a production build allows api.fresh_accounts.{key} at most {} a minute with a burst of {}, \
                     and the config sets {} with {}",
                    ceiling.per_minute,
                    ceiling.burst,
                    budget.per_minute,
                    budget.burst
                );
            }
        }
        Ok(())
    }
}
