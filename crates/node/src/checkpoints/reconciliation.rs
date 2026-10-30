//! Finalized checkpoint and collateral reconciliation.

use std::sync::atomic::Ordering;

use anyhow::{Context, ensure};
use engine_core::Fixed;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::checkpoints::{
    ApprovalId, CheckpointChain, Conflict, Health, LifecycleEvidence, Poster, VaultState, View, Work, signature,
};

/// The first vault deposit the finalized checkpoint has not credited, and since when it has been
/// seen so.
pub(super) type Uncredited = Option<(u64, Instant)>;

/// How the finalized vault state compares with the ledger `work` carries. The ledger's
/// liabilities are the cumulative credits; distributions reduce house cash in the engine before
/// the vault pays them, so the vault's balance plus what its claims have paid must cover what the
/// deposits left behind, and what it has paid cannot exceed what the engine allocated. A vault
/// counting deposits the checkpoint has not credited yet is routine catch-up, not divergence; the
/// caller applies the catch-up deadline.
fn verdict(state: &VaultState, paid: Fixed, credited: Option<Fixed>, work: &Work) -> Health {
    let distributed_raw = (work.distributed.0 as i128).saturating_mul(1_000);
    let overpaid = state.distributions_paid.raw() > distributed_raw;
    let valid_paid = paid >= Fixed::ZERO && paid <= work.authorized;
    let compensated = state.balance.raw().checked_add(state.distributions_paid.raw()).map(Fixed::from_raw);
    let held = state.deposited.raw().checked_sub(paid.raw()).map(Fixed::from_raw);
    let covered = held.is_some_and(|held| compensated.is_some_and(|sum| sum >= held));
    if overpaid || !valid_paid || !covered || credited != Some(work.liabilities) {
        Health::Diverged
    } else if state.deposits > work.deposits {
        Health::Pending
    } else {
        Health::Healthy
    }
}

/// Raises the Critical `vault_withdrawals_halted` for each way the finalized vault refuses
/// withdrawal claims, one episode per `cause`, and resolves each that has cleared.
fn report_halt(evidence: &LifecycleEvidence) {
    if evidence.withdrawals_paused {
        error!(
            alert = "vault_withdrawals_halted",
            cause = "paused",
            "withdrawal authorizations stopped: the vault's withdrawals are paused until its guardian's pause ends or its timelock lifts it"
        );
    } else {
        info!(alert_resolved = "vault_withdrawals_halted", cause = "paused", "the vault's withdrawals are not paused");
    }
    if evidence.signer_retired() {
        error!(
            alert = "vault_withdrawals_halted",
            cause = "signer_retired",
            "withdrawal authorizations stopped: the vault's guardian retired its signer, and only its timelock installs one"
        );
    } else {
        info!(alert_resolved = "vault_withdrawals_halted", cause = "signer_retired", "the vault has a signer");
    }
}

impl<R: CheckpointChain> Poster<R> {
    /// Verifies/extends the commitment and compares collateral to the checkpoint's ledger.
    /// Error cases:
    /// - the chain read or post fails, or a signer/root conflicts with the local log
    #[cfg(test)]
    pub(super) async fn reconcile(&self, work: &Work) -> anyhow::Result<(Option<u64>, Health)> {
        self.reconcile_known(work, &[work]).await
    }

    /// Matches public relays to the authenticated local chain before accepting a covered prefix.
    /// Error cases:
    /// - unavailable/lagging views, unknown progress, or a proved conflicting local commitment
    #[cfg(test)]
    pub(super) async fn reconcile_known(&self, work: &Work, known: &[&Work]) -> anyhow::Result<(Option<u64>, Health)> {
        let (posted, health, _) = self.reconcile_lifecycle(work, known, &[], &mut None).await?;
        Ok((posted, health))
    }

    /// Ends the `post_diverged` withholding episode, if one is open.
    fn resume_posting(&self) {
        if self.post_withheld.swap(false, Ordering::Relaxed) {
            info!(
                alert_resolved = "checkpoint_withheld",
                cause = "post_diverged",
                "checkpoint posting resumed: collateral no longer diverges"
            );
        }
    }

    /// Whether posting `work` must wait: the finalized vault state diverges from the ledger the
    /// finalized checkpoint carries. Read-only, so a withheld worker keeps reconciling and clears
    /// the moment collateral is restored, and a restarted worker — which remembers nothing —
    /// still refuses to extend the commitment into a proved shortfall. When the finalized
    /// checkpoint's work has not reached this worker, or its coverage still awaits the canonical
    /// scan, there is no verdict to act on and posting proceeds as before.
    async fn withholds(&self, known: &[&Work]) -> anyhow::Result<bool> {
        let observation = self.chain.observe(View::Finalized, &[]).await?;
        observation.validate(&[])?;
        let state = observation.state.clone();
        let Some((seq, root)) = &state.checkpoint else { return Ok(false) };
        let Some(work) = known.iter().copied().find(|work| work.checkpoint.seq == *seq) else { return Ok(false) };
        if work.checkpoint.root != *root || !work.has_proven_prefix() {
            return Ok(false);
        }
        let paid = observation.lifecycle.as_ref().map_or(Fixed::ZERO, |evidence| evidence.total_withdrawn);
        let credited = match work.deposits {
            0 => Some(Fixed::ZERO),
            count if count <= state.deposits => Some(self.chain.deposited_through(&observation, count).await?),
            _ => None,
        };
        Ok(matches!(verdict(&state, paid, credited, work), Health::Diverged))
    }

    /// The finalized evidence for `approvals`, alerted, when the finalized vault halts withdrawal
    /// claims under this enclave's authority and the latest block agrees: what a failed
    /// reconciliation still reports, so a stuck post never hides a pause or a retired signer. It
    /// halts withdrawals, so it releases nothing.
    /// Error cases:
    /// - an observation fails or is inconsistent
    pub(super) async fn observe_halt(&self, approvals: &[ApprovalId]) -> anyhow::Result<Option<LifecycleEvidence>> {
        let latest = self.chain.observe(View::Latest, &[]).await?;
        latest.validate(&[])?;
        let finalized = self.chain.observe(View::Finalized, approvals).await?;
        finalized.validate(approvals)?;
        let (Some(latest), Some(evidence)) = (latest.lifecycle, finalized.lifecycle) else { return Ok(None) };
        let agrees = latest.signer == evidence.signer
            && latest.signer_epoch == evidence.signer_epoch
            && latest.withdrawals_paused == evidence.withdrawals_paused;
        let authority = evidence.signer == self.signer || evidence.signer_retired();
        if !(agrees && authority && evidence.halts_withdrawals()) {
            return Ok(None);
        }
        report_halt(&evidence);
        Ok(Some(evidence))
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
    ) -> anyhow::Result<(Option<u64>, Health, Option<LifecycleEvidence>)> {
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
        // A retired signer is the guardian's halt, not another authority: the vault accepts no
        // checkpoint until its timelock installs a signer, and one other than this enclave conflicts.
        if state.signer != self.signer && !state.signer_retired() {
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
                if state.signer_retired() {
                    debug!("checkpoint not posted: the vault's signer is retired");
                } else if self.withholds(known).await? {
                    // The finalized vault diverges from the ledger: a new root would let
                    // distribution claims drain a vault the ledger says is short. Skip the post;
                    // the finalized read below still returns the verdict, so a repaired vault
                    // clears this on the next retry.
                    self.post_withheld.store(true, Ordering::Relaxed);
                    warn!(
                        alert = "checkpoint_withheld",
                        cause = "post_diverged",
                        "checkpoint not posted while collateral reconciliation is diverged"
                    );
                } else {
                    self.resume_posting();
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
        if state.signer != self.signer && !state.signer_retired() {
            return Err(Conflict("finalized vault signer differs from this enclave").into());
        }
        if let (Some(latest), Some(finalized)) = (&observation.lifecycle, &finalized_observation.lifecycle) {
            ensure!(latest.signer_epoch == finalized.signer_epoch, "signer epoch is not finalized");
            // No release evidence leaves while a pause or retirement is only in the latest block,
            // so either stops releases at once and only a finalized clearing resumes them.
            ensure!(
                latest.signer == finalized.signer && latest.withdrawals_paused == finalized.withdrawals_paused,
                "the vault's signer or withdrawal pause is not finalized"
            );
            report_halt(finalized);
        }
        // No finalized checkpoint has reached this worker's known chain yet. The finalized
        // lifecycle still reaches the sequencer — a retirement seen before the first finalized
        // checkpoint must withhold signing from the first observation — but it cannot release
        // an authorization: a bootstrap observation carries no checkpoint, so `release_payouts`
        // returns before any release, and a known-but-older finalized checkpoint implies no
        // confirmed checkpoint this process, so the gate's health is never Healthy.
        let Some((seq, root)) = &state.checkpoint else {
            return Ok((None, Health::Unvalidated, finalized_observation.lifecycle));
        };
        if *seq < known.iter().map(|work| work.checkpoint.seq).min().unwrap_or(cp.seq) {
            return Ok((None, Health::Unvalidated, finalized_observation.lifecycle));
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
        // The empty bootstrap ledger proves prefix zero without a scan. Every other unscanned
        // prefix remains unknown; a newer checkpoint cannot lend it canonical receipt coverage.
        if !work.has_proven_prefix() {
            warn!(
                alert = "collateral_pending_deposits",
                "withdrawals paused: the finalized checkpoint's custody prefix lacks sequenced canonical receipt coverage"
            );
            return Ok((Some(cp.seq), Health::Unproven, finalized_observation.lifecycle));
        }
        let paid = finalized_observation.lifecycle.as_ref().map_or(Fixed::ZERO, |evidence| evidence.total_withdrawn);
        let distributed_raw = (work.distributed.0 as i128).saturating_mul(1_000);
        let overpaid = state.distributions_paid.raw() > distributed_raw;
        // Deposits after the checkpoint's last credit are still in flight, so the ledger is
        // compared with the vault's running total through exactly the deposits it credited.
        let credited = match work.deposits {
            0 => Some(Fixed::ZERO),
            count if count <= state.deposits => {
                Some(self.chain.deposited_through(&finalized_observation, count).await?)
            }
            _ => None,
        };
        // The first in-flight deposit may stay uncredited only until the catch-up deadline; past
        // it, withdrawals stay paused and the escalation pages, but checkpoint signing and posting
        // continue: the vault is ahead, not short, and only a newer finalised checkpoint — the one
        // this gate would otherwise block — records the catch-up and clears the escalation. A
        // ledger disagreement (shortfall, overpaid distributions, a deposit-count mismatch) is a
        // true Diverged and freezes signing until the vault is repaired against this same
        // immutable checkpoint.
        // A retired signer posts no checkpoint, so none can record the catch-up: the halt alert
        // covers it, and the deadline starts once the timelock installs a signer.
        let first = (state.deposits > work.deposits && !state.signer_retired()).then_some(work.deposits);
        *uncredited = first.map(|id| match *uncredited {
            Some((tracked, since)) if tracked == id => (id, since),
            _ => (id, Instant::now()),
        });
        let overdue = uncredited.filter(|(_, since)| since.elapsed() >= self.pending_deposits_critical_after);
        let health = match verdict(state, paid, credited, work) {
            Health::Pending if overdue.is_none() => Health::Healthy,
            health => health,
        };
        if health == Health::Healthy {
            info!(alert_resolved = "collateral_mismatch", "vault collateral reconciles");
            info!(alert_resolved = "checkpoint_post_failed", "checkpoint is posted");
            info!(alert_resolved = "collateral_pending_deposits", "vault deposits are covered");
            self.resume_posting();
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
                distributions_paid = %state.distributions_paid,
                distributed = %work.distributed,
                "withdrawals paused: the vault holds less USDC than the ledger requires, or its deposits or payments differ from it"
            );
        }
        let compensated = state.balance.raw().checked_add(state.distributions_paid.raw()).map(Fixed::from_raw);
        let held = state.deposited.raw().checked_sub(paid.raw()).map(Fixed::from_raw);
        if held.is_some_and(|held| compensated.is_some_and(|sum| sum > held)) {
            warn!(
                alert = "collateral_surplus",
                "the vault holds USDC no deposit brought in; anyone may book 10 USDC or more of it as house capital with bookSurplus"
            );
        } else if health == Health::Healthy {
            info!(alert_resolved = "collateral_surplus", "the vault holds no unbooked USDC");
        }
        Ok((Some(cp.seq), health, finalized_observation.lifecycle))
    }
}
