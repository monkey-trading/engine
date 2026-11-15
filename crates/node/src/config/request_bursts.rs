//! `[api.request_bursts]`: how much of its tier's request rate a credited account may spend at once,
//! apart from the rate itself, which the account's volume tier sets (`volume_tier::limits`).

use std::num::NonZeroU32;

use anyhow::bail;
use serde::Deserialize;

use crate::config::ApiSection;

/// The most seconds of its rate a bucket may hold in a production build.
const PRODUCTION_BURST_SECONDS: NonZeroU32 = NonZeroU32::new(5).unwrap();

/// Seconds of the tier's rate each of a credited account's buckets holds: a bucket at 5 requests a
/// second holding 2 seconds takes 10 requests at once, then refills at 5 a second.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequestBursts {
    /// The bucket for the account's own requests.
    pub account: NonZeroU32,
    /// The bucket for the requests of all the account's session keys together.
    pub session_key: NonZeroU32,
}

impl ApiSection {
    /// Refuses bursts above the production value when `production` is set.
    /// Error cases:
    /// - `production` is set and a bucket holds more than the production value's seconds of its rate
    pub(super) fn refuse_dev_request_bursts(&self, production: bool) -> anyhow::Result<()> {
        let RequestBursts { account, session_key } = self.request_bursts;
        for (key, seconds) in [("account", account), ("session_key", session_key)] {
            if production && seconds > PRODUCTION_BURST_SECONDS {
                bail!(
                    "a production build allows api.request_bursts.{key} at most {PRODUCTION_BURST_SECONDS} seconds \
                     of the tier's rate, and the config sets {seconds}"
                );
            }
        }
        Ok(())
    }
}
