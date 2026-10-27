//! Independent finality proof for a legacy approval stranded before its signature.

use std::collections::BTreeMap;

use alloy::primitives::Address;
use anyhow::{anyhow, ensure};
use engine_core::Engine;
use engine_core::deposits::{DepositBeneficiary, LegacyDeposit};

use crate::config::EvmCustodySection;
use crate::custody::cursor::Cursor;
use crate::custody::evm::{DepositWatcher, VaultIdentity, VaultReader};
use crate::sequencer::legacy::{Approvals, Authorizations, FinalityProof};

impl<V: VaultReader> DepositWatcher<V> {
    /// Uses the same quorum and complete canonical transcript as receipt crediting, with no cursor.
    /// Error cases:
    /// - identity, tag, chain or receipt proof fails, or unresolved principal would fund the payout
    pub(crate) async fn prove_legacy_withdrawals(
        vault: V,
        cfg: &EvmCustodySection,
        chain_id: u64,
        address: Address,
        engine: &Engine,
        ledger: &BTreeMap<String, LegacyDeposit>,
        approvals: &Approvals,
    ) -> anyhow::Result<Authorizations> {
        let cursor = Cursor {
            chain_id,
            vault: address,
            next_block: cfg.start_block,
            next_deposit_id: 0,
            log_seq: None,
            last_block_hash: None,
            crediting: None,
        };
        let mut watcher = Self::from_cursor(vault, cfg, cursor, None);
        watcher.prove_legacy_approvals(engine, ledger, approvals).await
    }

    /// Proves inherited obligations with bounded consumed headers and no cash/state mutation.
    /// Error cases:
    /// - identity, canonical transcript or finality cannot establish every approval's funding
    pub(in crate::custody) async fn prove_legacy_approvals(
        &mut self,
        engine: &Engine,
        ledger: &BTreeMap<String, LegacyDeposit>,
        approvals: &Approvals,
    ) -> anyhow::Result<Authorizations> {
        self.chain = Default::default();
        ensure!(
            self.vault.identity().await? == (VaultIdentity { chain_id: self.cursor.chain_id, token: Some(self.usdc) }),
            "legacy finality proof has wrong chain or token"
        );
        let release = self.vault.withdrawal_finality().await?;
        let release = release.tagged.ok_or_else(|| anyhow!("legacy withdrawal has no finalised block"))?;
        self.prove_chain(self.start_block, release, release.number)
            .await
            .map_err(|e| anyhow!("legacy chain proof: {e:?}"))?;
        let (mut from, mut next_id, mut proof) = (self.start_block, 0, FinalityProof::default());
        while from <= release.number {
            let to = release.number.min(from.saturating_add(self.max_block_range - 1));
            let receipts =
                self.read_canonical(from, to, next_id).await.map_err(|e| anyhow!("legacy receipt proof: {e:?}"))?;
            next_id += receipts.len() as u64;
            for receipt in receipts {
                proof.include_pending_receipt(&receipt, engine.recorded_deposit(&receipt.reference()));
                let reference = format!("{}:{}", receipt.tx_hash, receipt.log_index);
                if let Some(old) = ledger.get(&reference) {
                    ensure!(
                        old.amount == receipt.amount && old.beneficiary == receipt.beneficiary,
                        "legacy canonical receipt changed beneficiary or amount"
                    );
                    if matches!(old.beneficiary, DepositBeneficiary::Trader { .. }) {
                        proof.legacy.insert(reference);
                    }
                }
            }
            from = to.checked_add(1).ok_or_else(|| anyhow!("legacy proof block overflow"))?;
        }
        let authorized = approvals.authorize(engine, ledger, &proof)?;
        ensure!(
            approvals.all_authorized(&authorized),
            "legacy withdrawal still depends on principal without canonical finality proof"
        );
        Ok(authorized)
    }
}
