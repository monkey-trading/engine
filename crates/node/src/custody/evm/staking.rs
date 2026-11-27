//! Finalized staking catch-up, independent of deposit scan progress.

use anyhow::anyhow;
use engine_core::staking::StakingCheckpoint;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::custody::credit::query;
use crate::custody::evm::{BlockRef, DepositWatcher, GovernanceRecord, VaultReader};
use crate::custody::staking;
use crate::custody::stall::PollError;
use crate::sequencer::Request;

impl<V: VaultReader> DepositWatcher<V> {
    /// The staking range now in force, for the adaptive-size regressions.
    #[cfg(test)]
    pub(crate) fn staking_range_for_test(&self) -> u64 {
        self.staking_range.get()
    }

    /// The deposit log range now in force, which staking failures must leave alone.
    #[cfg(test)]
    pub(crate) fn log_range_for_test(&self) -> u64 {
        self.log_range.get()
    }

    /// Mirrors the configured staking contract's events, and the operator registry's with them,
    /// through the Ethereum-finalised boundary this poll read (`release_at`), never the merely confirmed
    /// scan end: a staking mirror has no reversal, so a reorged event must never become a permanent engine
    /// balance. A window that ends at the block the release tag names, `release`, must end on the hash the
    /// quorum voted with the tag; one that ends below it, in a catch-up, is read with each provider's
    /// header there in the same voted answer, so it is bound to the tag's chain too. The authenticated
    /// frontier a window records is read again each poll. Catch-up runs in bounded chunks, the cursor
    /// advancing per chunk. Without a staking contract, a finalised boundary, or before `InitStaking` has
    /// applied, nothing is read or sequenced and the cursor holds (see `staking::mirror`). From `V93`, while a
    /// carried genesis's stakes are mirrored again, windows run from the staking contract's deployment
    /// (`custody.evm.staking.deployment_block`) to the source's frontier and no further, mirroring the staking
    /// contract's events alone, naming no turn, and paging nothing for a fault proof contract not yet deployed.
    /// Error cases:
    /// - stalled: a staking read fails, an event is not on the canonical block there, a mirrored contract is
    ///   not the one its parent or the schedule's governance record names, or the engine refuses a mirror
    /// - stalled: while the carried stakes are mirrored again, `remirror_start`'s stalls: no deployment block is
    ///   configured, or the registry has code before it
    /// - fatal: the sequencer has stopped, or the staking cursor cannot be stored
    pub(super) async fn mirror_staking(
        &mut self,
        tx: &mpsc::Sender<Request>,
        release: Option<BlockRef>,
        release_at: Option<u64>,
    ) -> Result<(), PollError> {
        let Some(mut to) = release_at else { return Ok(()) };
        if self.staking.is_none() {
            return Ok(());
        };
        let Some((anchor, mirrored, remirror)) = query(tx, |engine| {
            engine.staking().map(|staking| (staking.completed_through, staking.mirrored_through, staking.remirror))
        })
        .await
        .map_err(PollError::Fatal)?
        else {
            return Ok(());
        };
        let start = match remirror {
            Some(_) => self.remirror_start(anchor, mirrored).await?,
            None => self.start_block,
        };
        let from = anchor.map_or_else(|| mirrored.unwrap_or(start).max(start), |checkpoint| checkpoint.block + 1);
        if let Some(anchor) = anchor
            && self.header_at(anchor.block).await?.hash != anchor.block_hash
        {
            return Err(PollError::inconsistent(anyhow!("finalized staking anchor changed")));
        }
        if from > to {
            return Ok(());
        }
        // The cursor advances per completed chunk until it reaches the release tag, so catch-up reports
        // progress instead of restarting against a moving end. Event-read failures shrink the range (see
        // `staking::mirror`).
        to = to.min(from.saturating_add(self.staking_range.get() - 1));
        if let Some(pending) = remirror {
            to = to.min(pending.through.block);
        }
        let end = self.header_at(to).await?;
        if release.is_some_and(|release| release.number == to && release.hash != end.hash) {
            return Err(PollError::unsettled(anyhow!("the staking window's end is not the release tag's block")));
        }
        // The staking contract mirrored must be the one the configured registry records as the one it
        // deployed, whose elections mirror with it: a staking contract's own answer naming the registry
        // would bind any contract that answers so. A window ending before the registry's deployment has
        // no code to read and nothing that could name another staking contract, so the binding is checked
        // again at the end of a later window instead of trapping the catch-up on the same range.
        if let Some((registry, false)) = self.registry {
            let hash = end.hash;
            let staking =
                self.staking.as_ref().map(|(cursor, ..)| cursor.contract).expect("staking cursor checked above");
            match self.vault.registry_staking(BlockRef { number: to, hash }).await.map_err(PollError::rpc)? {
                None => info!(to, "the registry is not deployed at this window's end"),
                Some(recorded) => {
                    if recorded != staking {
                        return Err(PollError::inconsistent(anyhow!(
                            "the registry {registry} records staking contract {recorded}, not the configured {staking}"
                        )));
                    }
                    self.registry = Some((registry, true));
                }
            }
        }
        // The schedule mirrored must be the one the registry names, once it names one: a node that
        // mirrors another, or none, would never apply the turns that name the operator. A window that ends
        // before the registry moved custody to the configured schedule, as a log that carried another's state
        // reads from the carried frontier, names the one it followed then: the release tag's block must name
        // the configured one.
        if let Some((_, true)) = self.registry {
            let hash = end.hash;
            let named = self.vault.registry_schedule(BlockRef { number: to, hash }).await.map_err(PollError::rpc)?;
            let configured = self.schedule.map(|(schedule, _)| schedule);
            if !named.is_zero() && configured != Some(named) {
                let moved = match release.filter(|release| release.number > to) {
                    Some(release) => {
                        Some(self.vault.registry_schedule(release).await.map_err(PollError::rpc)?) == configured
                    }
                    None => false,
                };
                if !moved {
                    return Err(PollError::inconsistent(anyhow!(
                        "the registry names schedule {named}, but the config names {configured:?}"
                    )));
                }
            }
        }
        // So must the schedule's registry: the schedule's events mirror only from the registry's own.
        // Its turns must be this build's too: a schedule of another length names turn numbers whose
        // boundaries the engine would place elsewhere.
        if let (Some((schedule, false)), Some((registry, _))) = (self.schedule, self.registry) {
            let hash = end.hash;
            let at = BlockRef { number: to, hash };
            match self.vault.schedule_registry(at).await.map_err(PollError::rpc)? {
                None => info!(to, "the schedule is not deployed at this window's end"),
                Some(recorded) if recorded != registry => {
                    return Err(PollError::inconsistent(anyhow!(
                        "the schedule {schedule} records registry {recorded}, not the configured {registry}"
                    )));
                }
                Some(_) => {
                    let call = crate::custody::registry::turn_length_call();
                    let out = self.vault.contract_call(at, schedule, call).await.map_err(PollError::rpc)?;
                    if let Some(reason) = crate::custody::registry::turn_length_refusal(&out).map_err(PollError::rpc)? {
                        return Err(PollError::inconsistent(anyhow!("the schedule {schedule} is refused: {reason}")));
                    }
                    self.schedule = Some((schedule, true));
                }
            }
        }
        // So must the vote contract's schedule: its proposals vote by the schedule's sets.
        if let (Some((vote, false)), Some((schedule, _))) = (self.vote, self.schedule) {
            let hash = end.hash;
            let out = self
                .vault
                .contract_call(BlockRef { number: to, hash }, vote, crate::custody::registry::vote_schedule_call())
                .await
                .map_err(PollError::rpc)?;
            match crate::custody::registry::decode_vote_schedule(&out).map_err(PollError::rpc)? {
                None => info!(to, "the vote contract is not deployed at this window's end"),
                Some(recorded) if recorded != schedule => {
                    return Err(PollError::inconsistent(anyhow!(
                        "the vote contract {vote} records schedule {recorded}, not the configured {schedule}"
                    )));
                }
                Some(_) => self.vote = Some((vote, true)),
            }
        }
        // The fault proof contract must check proofs against this vault's signer history.
        if let Some((fault_proof, false)) = self.fault_proof {
            let hash = end.hash;
            let out = self
                .vault
                .contract_call(BlockRef { number: to, hash }, fault_proof, crate::custody::registry::fault_vault_call())
                .await
                .map_err(PollError::rpc)?;
            match crate::custody::registry::decode_fault_vault(&out).map_err(PollError::rpc)? {
                // Once the log records the address, for good, an address with no code mirrors no fault,
                // so it pages until the contract is deployed there; a window before the record, as in a
                // catch-up over blocks before the deployment, is no fault of the address.
                None => {
                    let recorded = query(tx, |engine| engine.operator_faults().config.is_some());
                    if remirror.is_none() && recorded.await.map_err(PollError::Fatal)? {
                        warn!(
                            alert = "fault_proof_undeployed",
                            contract = %fault_proof,
                            "the recorded fault proof contract has no code at block {to}"
                        );
                    } else {
                        info!(to, "the fault proof contract is not deployed at this window's end");
                    }
                }
                Some(recorded) if recorded != self.cursor.vault => {
                    return Err(PollError::inconsistent(anyhow!(
                        "the fault proof contract {fault_proof} records vault {recorded}, not the configured {}",
                        self.cursor.vault
                    )));
                }
                Some(_) => {
                    info!(alert_resolved = "fault_proof_undeployed", contract = %fault_proof, "fault proof contract bound");
                    self.fault_proof = Some((fault_proof, true));
                }
            }
        }
        // So must the vote and fault proof contracts be the ones the schedule's governance record names, once
        // it names any: a contract's own answer naming its parent would bind any contract that answers so. The
        // record is read at the release tag's block, the newest final one, so a catch-up over blocks from before
        // the timelock recorded them is held to the record once it is final, and only while the registry names
        // the configured schedule there. A schedule that keeps no record leaves each bound by its own answer.
        if !self.governance_settled
            && (self.vote.is_some() || self.fault_proof.is_some())
            && let Some((_, true)) = self.registry
        {
            let at = release.unwrap_or(BlockRef { number: to, hash: end.hash });
            let schedule = self.schedule.map(|(schedule, _)| schedule);
            if Some(self.vault.registry_schedule(at).await.map_err(PollError::rpc)?) == schedule {
                self.governance_settled = self.check_governance_record(at).await?;
            }
        }
        let (cursor, path, _) = self.staking.as_mut().expect("staking cursor checked above");
        let bound = self.registry.is_some_and(|(_, bound)| bound);
        let schedule_bound = self.schedule.is_none_or(|(_, bound)| bound);
        let vote_bound = self.vote.is_none_or(|(_, bound)| bound);
        // The registry's own state says which drawn turn, of the schedule it names, names the operator; the source's
        // blocks mirrored again name none.
        let turns =
            self.registry.and_then(|(registry, bound)| bound.then_some(registry)).filter(|_| remirror.is_none());
        let fault_bound = self.fault_proof.is_none_or(|(_, bound)| bound);
        staking::mirror(
            &self.vault,
            tx,
            cursor,
            path,
            &mut self.staking_range,
            (from, end, release),
            (bound, schedule_bound, vote_bound, fault_bound),
            (turns, remirror.is_some()),
        )
        .await
    }

    /// Whether the schedule's governance record, read at `at`, settles the configured vote and fault proof
    /// contracts for good: it names them, or the schedule keeps none. False while it names none yet. Each record
    /// read is published to the sequencer, which records the config's contracts in the log only while the
    /// latest names no other.
    /// Error cases:
    /// - stalled: the read fails, or the record names another vote or fault proof contract than the configured
    async fn check_governance_record(&self, at: BlockRef) -> Result<bool, PollError> {
        let record = self.vault.governance_record(at).await.map_err(PollError::rpc)?;
        if let Some(published) = &self.governance_record {
            published.send_replace(record);
        }
        let configured = (self.vote.map(|(vote, _)| vote), self.fault_proof.map(|(fault_proof, _)| fault_proof));
        if let Some(reason) = record.refusal(configured.0, configured.1) {
            return Err(PollError::inconsistent(anyhow!(reason)));
        }
        match record {
            GovernanceRecord::Absent => {
                info!("the schedule keeps no governance record: its contracts are bound by their own answers");
                Ok(true)
            }
            GovernanceRecord::Unset => {
                debug!(block = at.number, "the schedule's governance record names no contracts yet");
                Ok(false)
            }
            GovernanceRecord::Set { vote, fault_proof } => {
                info!(%vote, %fault_proof, "the schedule's governance record names the configured contracts");
                Ok(true)
            }
        }
    }

    /// Where a carried genesis's stakes are mirrored again from: the staking contract's deployment, before which
    /// the first window checks the registry that deploys it has no code.
    /// Error cases:
    /// - stalled: no deployment block is configured, or the registry that deploys the staking contract has code
    ///   before it, so the window would miss the staking contract's events
    /// - stalled: a read fails
    pub(in crate::custody) async fn remirror_start(
        &self,
        anchor: Option<StakingCheckpoint>,
        mirrored: Option<u64>,
    ) -> Result<u64, PollError> {
        let Some(deployed) = self.staking.as_ref().and_then(|(.., deployed)| *deployed) else {
            return Err(PollError::inconsistent(anyhow!(
                "the carried stakes are mirrored again from the staking contract's deployment, which \
                 custody.evm.staking.deployment_block must name"
            )));
        };
        if anchor.is_none() && mirrored.is_none() && deployed > 0 {
            let before = self.header_at(deployed - 1).await?;
            let at = BlockRef { number: before.number, hash: before.hash };
            // The registry deploys the staking contract in its own deployment, so it has code from the same block.
            if self.vault.registry_staking(at).await.map_err(PollError::rpc)?.is_some() {
                return Err(PollError::inconsistent(anyhow!(
                    "the registry, which deploys the staking contract, has code before \
                     custody.evm.staking.deployment_block {deployed}: mirroring the carried stakes again from there \
                     would miss the events before it"
                )));
            }
        }
        Ok(deployed)
    }
}
