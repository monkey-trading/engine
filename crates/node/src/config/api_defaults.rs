//! Defaults for `[api]` keys.

use std::num::NonZeroUsize;
use std::time::Duration;

use crate::config::MIN_QUOTE_INTERVAL;

/// Five seconds, the default `api.heartbeat_interval`.
pub(super) fn default_heartbeat_interval() -> Duration {
    Duration::from_secs(5)
}

pub(super) fn default_quote_interval() -> Duration {
    MIN_QUOTE_INTERVAL
}

/// Four, the default `api.quote_max_markets`.
pub(super) fn default_quote_max_markets() -> NonZeroUsize {
    NonZeroUsize::new(4).expect("four is not zero")
}
