//! Finalized checkpoint and collateral reconciliation.

use anyhow::{Context, ensure};
use engine_core::Fixed;
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::checkpoints::{
    ApprovalId, CheckpointChain, Conflict, DepositCoverage, LifecycleEvidence, Poster, View, Work, signature,
};

/// The first vault deposit the finalized checkpoint has not credited, and since when it has been
/// seen so.
pub(super) type Uncredited = Option<(u64, Instant)>;

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
        let (posted, healthy, _) = self.reconcile_lifecycle(work, known, &[], &mut None).await?;
        Ok((posted, healthy))
    }

    /// Returns release evidence only from the finalized collateral observation. `uncredited` carries
    /// the first uncredited vault deposit between calls.
    /// Error cases:
    /// - an observation, accepted identity, signed parent, or accounting boundary conflicts
    pub(super) async fn reconcile_lifecycle(
        &self,
        work: &Work,
        known: &[&Work],
        approvals: &[ApprovalId],
        uncredited: &mut Uncredited,
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
                return Err(anyhow::Error::from(Conflict("conflicting publicly relayed checkpoint root"))
                    .context(format!("vault root {root} at seq {seq}")));
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
                    return Err(anyhow::Error::from(Conflict("conflicting posted checkpoint root"))
                        .context(format!("vault root {root} at seq {seq}")));
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
            return Err(anyhow::Error::from(Conflict("finalized checkpoint conflicts with local log"))
                .context(format!("finalized vault root {root} at seq {seq}")));
        }
        let work = finalized;
        let cp = &work.checkpoint;
        let authorities = self.chain.accepted_checkpoints(&finalized_observation, &[cp.seq]).await?;
        ensure!(authorities.len() == 1, "finalized checkpoint authority is unavailable");
        authorities[0]
            .verify(cp)
            .map_err(|_| Conflict("finalized checkpoint signer conflicts with accepted history"))?;
        if work.coverage == DepositCoverage::AwaitingCanonicalScan {
            warn!(alert = "collateral_pending_deposits", "withdrawals await sequenced canonical receipt coverage");
            return Ok((Some(cp.seq), false, finalized_observation.lifecycle));
        }
        let paid = finalized_observation.lifecycle.as_ref().map_or(Fixed::ZERO, |evidence| evidence.total_withdrawn);
        let valid_paid = paid >= Fixed::ZERO && paid <= work.authorized;
        // What the vault's deposits brought in less what it paid out. It may hold more, when USDC
        // arrived outside a deposit, but never less.
        let held = state.deposited.raw().checked_sub(paid.raw()).map(Fixed::from_raw);
        // Distributions leave the vault when stakers claim, but the engine set their backing aside
        // from house cash when each epoch closed, so what claims have paid explains balance the
        // deposits do not: the balance plus the distributions paid must cover what the deposits
        // left behind, and the vault can never have paid more than the engine allocated.
        let distributed_raw = (work.distributed.0 as i128).saturating_mul(1_000);
        let overpaid = state.distributions_paid.raw() > distributed_raw;
        let compensated = state.balance.raw().checked_add(state.distributions_paid.raw()).map(Fixed::from_raw);
        let covered = held.is_some_and(|held| compensated.is_some_and(|sum| sum >= held));
        // Deposits after the checkpoint's last credit are still in flight, so the ledger is
        // compared with the vault's running total through exactly the deposits it credited.
        let credited = match work.deposits {
            0 => Some(Fixed::ZERO),
            count if count <= state.deposits => {
                Some(self.chain.deposited_through(&finalized_observation, count).await?)
            }
            _ => None,
        };
        // The first in-flight deposit may stay uncredited only until the catch-up deadline, so a
        // deposit custody skipped still pauses withdrawals.
        let first = (state.deposits > work.deposits).then_some(work.deposits);
        *uncredited = first.map(|id| match *uncredited {
            Some((tracked, since)) if tracked == id => (id, since),
            _ => (id, Instant::now()),
        });
        let overdue = uncredited.filter(|(_, since)| since.elapsed() >= self.pending_deposits_critical_after);
        let healthy = !overpaid && valid_paid && credited == Some(work.liabilities) && covered && overdue.is_none();
        if healthy {
            info!(alert_resolved = "collateral_mismatch", "vault collateral reconciles");
            info!(alert_resolved = "checkpoint_post_failed", "checkpoint is posted");
            info!(alert_resolved = "collateral_pending_deposits", "vault deposits are covered");
        } else if overpaid {
            error!(
                alert = "collateral_mismatch",
                distributions_paid = %state.distributions_paid,
                distributed = %work.distributed,
                "withdrawals paused: the vault paid more in distributions than the engine allocated"
            );
        } else if let Some((id, _)) = overdue {
            error!(
                alert = "collateral_mismatch",
                "withdrawals paused: vault deposit {id} is still uncredited after the catch-up deadline"
            );
        } else {
            error!(
                alert = "collateral_mismatch",
                "withdrawals paused: the vault holds less USDC than the ledger requires, or its deposits or payments differ from it"
            );
        }
        if held.is_some_and(|held| compensated.is_some_and(|sum| sum > held)) {
            warn!(
                alert = "collateral_surplus",
                "the vault holds USDC no deposit brought in; anyone may book 10 USDC or more of it as house capital with bookSurplus"
            );
        } else if covered {
            info!(alert_resolved = "collateral_surplus", "the vault holds no unbooked USDC");
        }
        Ok((Some(cp.seq), healthy, finalized_observation.lifecycle))
    }
}
