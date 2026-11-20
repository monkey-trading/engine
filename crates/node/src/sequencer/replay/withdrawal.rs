//! A withdrawal entry's replay: its signature against the authority the log and the replay's trust give,
//! and the approval it releases.

use anyhow::{Context, bail, ensure};

use crate::log::CheckpointDomain;
use crate::sequencer::SignedWithdrawal;
use crate::sequencer::replay::Replay;

impl Replay {
    /// Applies a withdrawal entry, the work of `apply_entry` for one.
    /// Error cases:
    /// - the trusted deployment differs from the recorded one, or the entry's signature does not verify
    /// - a typed payout names no approval, another amount or approval seq, or a checkpoint the log does not
    ///   hold before it
    /// - an untyped one matches neither a deferred approval nor the next unmatched one its command made
    pub(super) fn apply_withdrawal(&mut self, w: &SignedWithdrawal) -> anyhow::Result<()> {
        let engine = &self.engine;
        let recorded = engine
            .operator()
            .map(|operator| CheckpointDomain { chain_id: operator.chain_id, vault: operator.vault })
            .or_else(|| match engine.client_auth() {
                Some(engine_core::auth::ClientAuth::Eip712 { chain_id, vault }) => {
                    Some(CheckpointDomain { chain_id: *chain_id, vault: *vault })
                }
                _ => None,
            });
        if let (Some(explicit), Some(recorded)) = (self.domain, recorded) {
            ensure!(explicit == recorded, "withdrawal trusted deployment differs from recorded deployment");
        }
        let authority = if let Some(auth) = &w.authorization {
            if self.signer_epochs.is_empty() {
                self.trusted_signer
            } else {
                ensure!(
                    self.authorities
                        .get(&auth.checkpoint_seq)
                        .is_some_and(|accepted| accepted.root == auth.checkpoint_root.to_string()),
                    "typed payout root was not accepted in the finalized prefix"
                );
                let signer = self
                    .signer_epochs
                    .get(&auth.signer_epoch)
                    .context("typed payout signer epoch lacks authenticated history")?;
                ensure!(*signer == auth.signer, "typed payout signer differs from immutable epoch history");
                Some(*signer)
            }
        } else {
            self.authorities.range(engine.last_seq().unwrap_or(0)..).next().map(|(_, accepted)| accepted.signer)
        };
        let signer = authority.or(self.trusted_signer).or_else(|| {
            // A pure multi-signer audit has no single pinned authority: a typed withdrawal
            // authenticates against its own authorization's signer, which the attested set
            // must name.
            let auth = w.authorization.as_ref()?;
            self.attested.as_ref()?.contains(&auth.signer).then_some(auth.signer)
        });
        crate::checkpoints::signature::verify_withdrawal(w, self.domain.or(recorded), signer)?;
        if let Some(auth) = &w.authorization {
            let approval = engine.withdrawal_approval(w.account, w.nonce).context("typed payout was never approved")?;
            ensure!(
                approval.amount == w.amount && Some(approval.approved_seq) == w.approved_seq,
                "typed payout differs from its immutable approval"
            );
            if self.payout_checkpoints.get(&auth.checkpoint_seq) != Some(&auth.checkpoint_root) {
                // A checkpoint before a range's start is the pass over the ranges' to find (`ranges`).
                match self.range.as_mut().filter(|range| range.start.is_some_and(|start| auth.checkpoint_seq < start)) {
                    Some(range) => range.payouts.push((auth.checkpoint_seq, auth.checkpoint_root)),
                    None => bail!("typed payout checkpoint is absent from the preceding log"),
                }
            }
            self.signed.insert((w.account, w.nonce));
            self.authorized.insert((w.account, w.nonce), w.clone());
            return Ok(());
        }
        if let Some(seq) = w.approved_seq {
            let deferred = (seq, w.account, w.amount, w.nonce);
            match self.deferred.iter().position(|value| *value == deferred) {
                Some(at) => drop(self.deferred.remove(at)),
                // A checkpoint at or before a range's start deferred it in an earlier range (`ranges`).
                None => match self.range.as_mut().filter(|range| range.start.is_some_and(|start| seq <= start)) {
                    Some(range) => range.deferred.push(deferred),
                    None => bail!("deferred withdrawal was not approved"),
                },
            }
            self.legacy_approvals.signed(w.account, w.amount, w.nonce);
            self.signed.insert((w.account, w.nonce));
            return Ok(());
        }
        if self.approved.front() != Some(&(w.account, w.amount, w.nonce)) {
            bail!("withdrawal {} nonce {} was not approved at seq {:?}", w.account, w.nonce, engine.last_seq());
        }
        self.approved.pop_front();
        self.legacy_approvals.signed(w.account, w.amount, w.nonce);
        self.signed.insert((w.account, w.nonce));
        Ok(())
    }
}
