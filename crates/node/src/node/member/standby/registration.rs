//! The registration file: the vouched `registerStandby` call the candidate sends from its own account.

use alloy::primitives::{Address, Bytes};
use alloy::sol_types::SolCall;
use anyhow::Context;
use engine_core::types::AccountId;
use serde::Serialize;
use tracing::info;

use crate::liveness::chain::registerStandbyCall;
use crate::node::member::standby::Standby;
use crate::standby_key::vouch::StandbyRegistration;

/// The registration file the candidate sends from.
pub(super) const REGISTRATION: &str = "standby-registration.json";

/// The registration as the file carries it.
#[derive(Serialize)]
struct Written<'a> {
    candidate: AccountId,
    schedule: Address,
    key: AccountId,
    nonce: u64,
    expiry: u64,
    voucher: AccountId,
    vouch: &'a engine_core::eip712::Signature,
    /// `registerStandby(key, nonce, expiry, vouch)`, to send from the candidate's account.
    calldata: Bytes,
}

impl Standby {
    /// Writes `registration` and its call to the registration file, returning the call's hex.
    /// Error cases:
    /// - the file cannot be written
    pub(super) fn write(&self, registration: &StandbyRegistration) -> anyhow::Result<String> {
        let calldata: Bytes = registerStandbyCall {
            key: Address::from(registration.key.0),
            nonce: registration.nonce,
            expiry: registration.expiry,
            vouch: Bytes::from(registration.vouch.to_vec()),
        }
        .abi_encode()
        .into();
        let written = Written {
            candidate: self.candidate,
            schedule: self.domain.schedule,
            key: registration.key,
            nonce: registration.nonce,
            expiry: registration.expiry,
            voucher: registration.voucher,
            vouch: &registration.vouch,
            calldata: calldata.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&written).context("encoding the registration")?;
        crate::tee::sealing::write_replacing(&self.dir.join(REGISTRATION), &bytes)?;
        Ok(calldata.to_string())
    }
}

/// Removes the registration file from `dir`, if one is there.
/// Error cases:
/// - the file is there and cannot be removed, or the directory cannot be synced
pub(super) fn remove_registration(dir: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::remove_file(dir.join(REGISTRATION)) {
        Ok(()) => {
            std::fs::File::open(dir).and_then(|dir| dir.sync_all()).context("syncing the sealing directory")?;
            info!("removed the registration written for the standby key this one replaced");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("removing the stale standby registration"),
    }
}
