//! Legacy upgrade inputs come from verified commands; unsigned payouts need independent finality.

use std::collections::BTreeMap;

use anyhow::{Context, bail, ensure};
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::LegacyDeposit;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};

use tracing::{error, info};

use crate::config::{CustodyProvider, CustodySection};
use crate::custody;
use crate::log::LogEntry;
use crate::sequencer::{ReplayedLog, Sequencer, SignedWithdrawal};

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

/// A cache carries neither the legacy credit ledger nor inherited approval provenance; only full
/// replay derives them from verified commands, so a cache that would need them is refused and
/// recovery replays the whole log instead.
/// Error cases:
/// - the cached engine predates custody finality, or owes an approval made before it activated
pub(super) fn check_snapshot_provenance(
    engine: &Engine,
    activations: &[(ProtocolVersion, u64)],
    deferred: &[(u64, AccountId, Fixed, u64)],
) -> anyhow::Result<()> {
    ensure!(engine.deposit_finality_enabled(), "snapshot predates custody finality; legacy credits need full replay");
    let activated = activations.iter().find(|(version, _)| version.supports_deposit_finality()).map(|(_, seq)| *seq);
    ensure!(
        deferred.iter().all(|(seq, ..)| activated.is_some_and(|activated| *seq >= activated)),
        "snapshot owes a withdrawal approved before custody finality; its provenance needs full replay"
    );
    Ok(())
}

impl Sequencer {
    /// Signs an approved withdrawal and logs it; custody and the stream get it once it is flushed.
    /// Error cases:
    /// - the log cannot be written
    pub(super) fn sign_withdrawal(&mut self, account: AccountId, amount: Fixed, nonce: u64) -> anyhow::Result<()> {
        let seq = self.engine.last_seq().context("withdrawal without approval sequence")?;
        if self.withdrawal_release_seq(account, nonce, seq, None)?.is_none() {
            error!(alert = "legacy_withdrawal_unproven", %account, nonce, "inherited withdrawal lacks canonical finality proof");
            bail!("inherited withdrawal lacks canonical finality proof");
        }
        self.log.sync()?;
        let signature = self.key.sign_withdrawal(self.chain_id, &self.vault.0, account, amount, nonce);
        info!(%account, nonce, "withdrawal signed");
        self.record(LogEntry::Withdrawal(SignedWithdrawal {
            account,
            amount,
            nonce,
            chain_id: self.chain_id,
            vault: self.vault,
            signature,
            approved_seq: None,
            authorization: None,
        }))?;
        self.legacy_approvals.signed(account, amount, nonce);
        self.legacy_authorizations.remove(&(account, nonce));
        Ok(())
    }
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
