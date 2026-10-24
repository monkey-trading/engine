//! Finalized checkpoint and collateral reconciliation.

use anyhow::{Context, ensure};
use tracing::{error, info, warn};

use crate::checkpoints::{ApprovalId, CheckpointChain, Conflict, LifecycleEvidence, Poster, View, Work, signature};

impl<R: CheckpointChain> Poster<R> {
    /// Verifies/extends the commitment and compares collateral to the checkpoint's ledger.
    /// Error cases:
    /// - the chain read or post fails, or a signer/root conflicts with the local log
    #[cfg(test)]
    pub(super) async fn reconcile(&self, work: &Work) -> anyhow::Result<(Option<u64>, bool)> {
        self.reconcile_known(work, &[work]).await
    }

    /// Matches public relays to the authenticated local chain before accepting a covered prefix.
    /// Error cases:
    /// - unavailable/lagging views, unknown progress, or a proved conflicting local commitment
    #[cfg(test)]
    pub(super) async fn reconcile_known(&self, work: &Work, known: &[&Work]) -> anyhow::Result<(Option<u64>, bool)> {
        let (posted, healthy, _) = self.reconcile_lifecycle(work, known, &[]).await?;
        Ok((posted, healthy))
    }

    /// Returns release evidence only from the finalized collateral observation.
    /// Error cases:
    /// - an observation, accepted identity, signed parent, or accounting boundary conflicts
    pub(super) async fn reconcile_lifecycle(
        &self,
        work: &Work,
        known: &[&Work],
        approvals: &[ApprovalId],
    ) -> anyhow::Result<(Option<u64>, bool, Option<LifecycleEvidence>)> {
        let mut work = work;
        let mut observation = self.chain.observe(View::Latest, &[]).await?;
        observation.validate(&[])?;
        let mut state = observation.state.clone();
        tracing::Span::current()
            .record("latest_seq", tracing::field::debug(state.checkpoint.as_ref().map(|(seq, _)| *seq)));
        if let Some((seq, root)) = &state.checkpoint
            && *seq > work.checkpoint.seq
        {
            work = known
                .iter()
                .copied()
                .find(|work| work.checkpoint.seq == *seq)
                .context("vault advanced to a checkpoint not yet received by this worker")?;
            if work.checkpoint.root != *root {
                return Err(Conflict("conflicting publicly relayed checkpoint root").into());
            }
        }
        let cp = &work.checkpoint;
        signature::verify_domain(cp, self.domain)?;
        if state.signer != self.signer {
            return Err(Conflict("checkpoint signer differs from the authorized vault signer").into());
        }
        match &state.checkpoint {
            Some((seq, root)) if *seq == cp.seq => {
                if *root != cp.root {
                    return Err(Conflict("conflicting posted checkpoint root").into());
                }
            }
            Some((seq, _)) if *seq > cp.seq => {
                return Err(Conflict("vault advanced beyond the local checkpoint").into());
            }
            _ => {
                if cp.signer.parse::<engine_core::types::AccountId>()? != self.signer {
                    return Err(Conflict("unposted checkpoint signer differs from the authorized vault signer").into());
                }
                let parent = cp.parent.context("on-chain checkpoints require signed parent metadata")?;
                let expected = state
                    .checkpoint
                    .as_ref()
                    .map(|(seq, root)| -> anyhow::Result<_> {
                        Ok(crate::log::CheckpointParent {
                            has_checkpoint: true,
                            seq: *seq,
                            root: root.parse::<engine_core::eip712::B256>()?,
                        })
                    })
                    .transpose()?
                    .unwrap_or_default();
                if parent != expected {
                    ensure!(
                        !(parent.has_checkpoint && (!expected.has_checkpoint || expected.seq < parent.seq)),
                        "checkpoint view is behind the signed predecessor; awaiting backend catch-up"
                    );
                    return Err(Conflict("signed parent differs from the latest vault checkpoint").into());
                }
                self.chain.post(cp).await?;
                // A submitted transaction is not a commitment. Verify inclusion before release.
                observation = self.chain.observe(View::Latest, &[]).await?;
                observation.validate(&[])?;
                state = observation.state.clone();
                ensure!(
                    state.checkpoint == Some((cp.seq, cp.root.clone())) && state.signer == self.signer,
                    "checkpoint transaction is not included in the authorized vault"
                );
            }
        }
        // Latest inclusion prevents duplicate submission, but is revocable by a shallow reorg.
        tracing::Span::current()
            .record("latest_seq", tracing::field::debug(state.checkpoint.as_ref().map(|(seq, _)| *seq)));
        let included_height = state.height;
        let finalized_observation = self.chain.observe(View::Finalized, approvals).await?;
        finalized_observation.validate(approvals)?;
        let state = &finalized_observation.state;
        tracing::Span::current()
            .record("finalized_seq", tracing::field::debug(state.checkpoint.as_ref().map(|(seq, _)| *seq)));
        ensure!(included_height >= state.height, "latest checkpoint view is behind the finalized view");
        if state.signer != self.signer {
            return Err(Conflict("finalized vault signer differs from this enclave").into());
        }
        if let (Some(latest), Some(finalized)) = (&observation.lifecycle, &finalized_observation.lifecycle) {
            ensure!(latest.signer_epoch == finalized.signer_epoch, "signer epoch is not finalized");
        }
        let Some((seq, root)) = &state.checkpoint else { return Ok((None, false, None)) };
        if *seq < known.iter().map(|work| work.checkpoint.seq).min().unwrap_or(cp.seq) {
            return Ok((None, false, None));
        }
        let finalized = known
            .iter()
            .copied()
            .find(|work| work.checkpoint.seq == *seq)
            .context("finalized checkpoint has not yet reached this worker")?;
        signature::verify_domain(&finalized.checkpoint, self.domain)?;
        if finalized.checkpoint.root != *root {
            return Err(Conflict("finalized checkpoint conflicts with local log").into());
        }
        let work = finalized;
        let cp = &work.checkpoint;
        let authorities = self.chain.accepted_checkpoints(&finalized_observation, &[cp.seq]).await?;
        ensure!(authorities.len() == 1, "finalized checkpoint authority is unavailable");
        authorities[0]
            .verify(cp)
            .map_err(|_| Conflict("finalized checkpoint signer conflicts with accepted history"))?;
        let paid = finalized_observation
            .lifecycle
            .as_ref()
            .map_or(engine_core::Fixed::ZERO, |evidence| evidence.total_withdrawn);
        let valid_paid = paid >= engine_core::Fixed::ZERO && paid <= work.authorized;
        let expected = work.liabilities.raw().checked_sub(paid.raw()).map(engine_core::Fixed::from_raw);
        let healthy = valid_paid && expected == Some(state.balance) && state.deposits == work.deposits;
        if healthy {
            info!(alert_resolved = "collateral_mismatch", "vault collateral reconciles");
            info!(alert_resolved = "checkpoint_post_failed", "checkpoint is posted");
            info!(alert_resolved = "collateral_pending_deposits", "vault deposits are credited");
        } else if valid_paid
            && state.deposits > work.deposits
            && expected.is_some_and(|expected| state.balance >= expected)
        {
            warn!(
                alert = "collateral_pending_deposits",
                "withdrawals await the checkpoint covering final deposit credits"
            );
        } else {
            error!(
                alert = "collateral_mismatch",
                "withdrawals paused: vault collateral or deposit count differs from ledger"
            );
        }
        Ok((Some(cp.seq), healthy, finalized_observation.lifecycle))
    }
}
