//! The attestation of a member's standby key its link answers with: the latest vouch request the
//! member quoted for it, which the operator checks while the key is not registered.

use std::sync::PoisonError;

use anyhow::Context;
use engine_core::types::AccountId;

use crate::liveness::link::Copy;
use crate::standby_key::vouch::VouchRequest;

impl Copy {
    /// Records `request`, the latest vouch request the member quoted for its standby key.
    pub(crate) fn attested(&self, request: &VouchRequest) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).attestation = Some(request.clone());
    }

    /// The latest vouch request quoted for `key`, the standby key the member link signs with.
    /// Error cases:
    /// - none was quoted for that key yet
    pub(crate) fn attestation(&self, key: AccountId) -> anyhow::Result<Box<VouchRequest>> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let attested = state.attestation.clone().filter(|request| request.key == key);
        attested.map(Box::new).with_context(|| format!("no attestation of the standby key {key} yet"))
    }
}
