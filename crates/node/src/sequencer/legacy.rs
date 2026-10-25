//! Legacy upgrade inputs come from verified commands; unsigned payouts need independent finality.

use std::collections::BTreeMap;

use anyhow::{bail, ensure};
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::LegacyDeposit;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::config::{CustodyProvider, CustodySection};
use crate::custody;
use crate::sequencer::{ReplayedLog, Sequencer};

mod approvals;
pub(crate) use approvals::{Approvals, Authorizations, FinalityProof};

/// Checks the activation input against the successful credits in the actual verified log.
/// Error cases:
/// - an activation invents, omits, duplicates or changes a legacy credit
pub(super) fn check_activation_ledger(
    command: &Command,
    ledger: &BTreeMap<String, LegacyDeposit>,
) -> anyhow::Result<()> {
    if let Command::Admin(AdminCommand::ActivateCustodyFinality { legacy_deposits, .. }) = command {
        ensure!(
            *legacy_deposits == ledger.values().cloned().collect::<Vec<_>>(),
            "legacy custody activation differs from the verified credit ledger"
        );
    }
    Ok(())
}

impl ReplayedLog {
    /// Proves any stranded historical approval safe before a writer or signature is created.
    /// Error cases:
    /// - missing EVM config, unavailable quorum, changed receipts or insufficient finality proof
    pub(crate) async fn prove_legacy_withdrawals(&mut self, cfg: &CustodySection) -> anyhow::Result<()> {
        if !ProtocolVersion::LATEST.supports_deposit_finality() {
            return Ok(());
        }
        let r = &self.replayed;
        let authorized = r.legacy_approvals.authorize(&r.engine, &r.legacy_deposits, &FinalityProof::default())?;
        if r.legacy_approvals.all_authorized(&authorized) {
            self.legacy_authorizations = authorized;
            return Ok(());
        }
        if cfg.provider != CustodyProvider::Evm {
            bail!("legacy EVM withdrawal requires independently proven canonical finality");
        }
        self.legacy_authorizations =
            custody::prove_legacy_withdrawals(cfg, &r.engine, &r.legacy_deposits, &r.legacy_approvals).await?;
        Ok(())
    }
}

impl Sequencer {
    /// Binds inherited release to its proved funding prefix, preserving the original approval tuple.
    /// Error cases:
    /// - principal cannot be checked without overflow
    pub(super) fn withdrawal_release_seq(
        &self,
        account: AccountId,
        nonce: u64,
        seq: u64,
        batch: Option<&Authorizations>,
    ) -> anyhow::Result<Option<u64>> {
        if !ProtocolVersion::LATEST.supports_deposit_finality() || !self.legacy_approvals.contains(account, nonce) {
            return Ok(Some(seq));
        }
        if self.legacy_authorizations.contains_key(&(account, nonce)) {
            return Ok(self.legacy_approvals.release_seq(&self.legacy_authorizations, account, nonce, seq));
        }
        if let Some(authorized) = batch {
            return Ok(self.legacy_approvals.release_seq(authorized, account, nonce, seq));
        }
        let authorized =
            self.legacy_approvals.authorize(&self.engine, &self.legacy_deposits, &FinalityProof::default())?;
        Ok(self.legacy_approvals.release_seq(&authorized, account, nonce, seq))
    }
}

#[cfg(test)]
mod tests;
