//! The standby's config: what it follows and how it watches for its promotion.

use std::time::Duration;

use serde::Deserialize;

/// Standby mode: follow the operator in effect and take over when the registry promotes this
/// node's candidate after the liveness timeout.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandbySection {
    /// The operator in effect's API origin, for its `/v1/stream`; the archive is read from
    /// `mirrors` first and the operator itself last, since the operator may be dead exactly when
    /// the archive matters. Every origin must serve the same public bytes.
    pub operator: String,
    /// Archive mirrors (#48's) to backfill and refresh from, tried in order before `operator`.
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// The enclave address the operator's verified attestation names; every stream frame verifies
    /// against it, so the host that carries the connection authenticates nothing.
    pub enclave: engine_core::types::AccountId,
    /// How often the vault and registry are polled for this candidate's promotion.
    #[serde(default = "StandbySection::default_poll_interval", with = "humantime_serde")]
    pub poll_interval: Duration,
    /// Accept the operator's self-signed or private-CA TLS certificate. Development only: the
    /// enclave-keyed frame signatures remain the authenticity boundary; a production build
    /// refuses this.
    #[serde(default)]
    pub danger_accept_invalid_certs: bool,
}

impl StandbySection {
    fn default_poll_interval() -> Duration {
        Duration::from_secs(30)
    }
}
