//! Canonical reconciliation of sequencer-confirmed receipts against Ethereum finality.

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::B256;
use anyhow::anyhow;
use engine_core::access::receipt_access_deposit;
use engine_core::command::Command;
use engine_core::deposits::{
    CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate, RecordedDeposit,
};
use engine_core::eip712::deposit_screening_hash;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use tokio::sync::mpsc;
use tracing::{Instrument, debug, error, info, info_span};

use crate::access::{ScreeningRequest, screen};
use crate::custody::credit::{query, submit};
use crate::custody::evm::{
    Beneficiary, BlockRef, DepositWatcher, VaultDeposit, VaultIdentity, VaultReader, check_complete,
};
use crate::custody::stall::{PollError, Polled};
use crate::custody::usdc_to_fixed;
use crate::sequencer::Request;

mod revision;
mod transitions;
use revision::{observed_revision, refusal_stall, refused};
use transitions::raced_activation;

impl<V: VaultReader> DepositWatcher<V> {
    /// Credits canonical confirmations, reconciles pending receipts, and releases final principal.
    /// Error cases:
    /// - a read lacks quorum, canonical observations disagree, or the sequencer refuses a transition
    pub(super) async fn poll_confirmed(&mut self, tx: &mpsc::Sender<Request>) -> Result<Polled, PollError> {
        let identity = self.vault.identity().await.map_err(PollError::rpc)?;
        if identity != (VaultIdentity { chain_id: self.cursor.chain_id, token: Some(self.usdc) }) {
            return Err(PollError::inconsistent(anyhow!("custody quorum disagrees with configured chain or token")));
        }
        let confirmation = self.vault.finality().await.map_err(PollError::rpc)?;
        let release = self.vault.withdrawal_finality().await.map_err(PollError::rpc)?;
        let eligible = confirmation.tagged.and_then(|tag| tag.number.checked_sub(self.confirmations));
        self.watchdog.observe(
            release.head,
            release.head_time,
            release.tagged.map(|tag| tag.number),
            release.head_support.as_ref(),
        );
        let (Some(tagged), Some(eligible)) = (confirmation.tagged, eligible) else {
            return Ok(Polled::Waiting);
        };
        let checkpoint = query(tx, |engine| engine.custody_checkpoint()).await.map_err(PollError::Fatal)?;
        let from = checkpoint.map_or(self.start_block, |p| p.block + 1);
        let next_id = checkpoint.map_or(0, |p| p.deposit_count);
        // Replayed progress anchors finality; a host cursor cannot skip receipt checks.
        self.cursor.next_block = from;
        self.cursor.next_deposit_id = next_id;
        self.cursor.last_block_hash = checkpoint.map(|p| p.block_hash);
        self.cursor.crediting = None;
        if let Some(checkpoint) = checkpoint {
            let headers = self.vault.headers(checkpoint.block, checkpoint.block).await.map_err(PollError::rpc)?;
            if headers.len() != 1 || headers[0].number != checkpoint.block || headers[0].hash != checkpoint.block_hash {
                return Err(PollError::inconsistent(anyhow!("quorum changed an already finalised custody block")));
            }
            self.chain.insert(checkpoint.block, checkpoint.block_hash);
            self.chain.prune_below(checkpoint.block);
        }
        if eligible < from {
            let behind =
                self.mirror_staking(tx, release.tagged, release.tagged.map(|tag| tag.number.min(eligible))).await?;
            return Ok(if behind { Polled::Read { caught_up: false } } else { Polled::Waiting });
        }
        let release_at = if let Some(released) = release.tagged {
            if released.number > tagged.number {
                return Err(PollError::unsettled(anyhow!("withdrawal finality is ahead of the confirmation view")));
            }
            Some(released.number.min(eligible))
        } else {
            None
        };
        let pending = query(tx, move |engine| engine.unfinalised_deposit_records(from, u64::MAX))
            .await
            .map_err(PollError::Fatal)?;
        self.resume_confirmed_scan(checkpoint, from, next_id)?;
        // Each provider reads the boundary after its window, in one voted answer, and the window joins the
        // scan only once the boundary holds. A window that fails is checked too, by a read of the boundary
        // alone, as a reorganisation below it can be why.
        let floor = checkpoint.map(|p| p.block);
        let boundary = self.chain.top().filter(|(top, _)| floor.is_none_or(|floor| *top > floor));
        // A lagging quorum tag below the boundary leaves nothing to read, and lagging providers may not
        // serve the boundary at all, so the poll waits out the tag without arming the lag watchdog.
        if boundary.is_some_and(|(top, _)| top > eligible) {
            return Ok(Polled::Waiting);
        }
        let window = self.read_window(eligible, boundary.map(|(number, _)| number)).await;
        let observed = window.as_ref().ok().and_then(Option::as_ref).and_then(|window| window.boundary);
        if !self.check_boundary(floor, boundary, observed).await? {
            return Ok(self.behind(from, eligible));
        }
        if let Some(window) = window? {
            self.commit_window(window);
        }
        let to = self.confirmed.scan.as_ref().expect("scan retained").end.expect("a complete window exists").number;
        // A lagging quorum tag drops the eligible block below the scan end or an already credited receipt;
        // the scan is ahead, so this waits out the tag without arming the lag watchdog, and the poll loop
        // sleeps its normal interval.
        if to > eligible || pending.iter().any(|record| record.receipt.block > eligible) {
            return Ok(Polled::Waiting);
        }
        // Even a replacement moved to an earlier block waits for all inherited pending
        // identities to be covered. An older quorum head proves no absence above itself.
        if pending.iter().any(|record| record.receipt.block > to) {
            return Ok(self.behind(from, eligible));
        }
        // A pending receipt in a block no window returned a deposit in has its block's hash read apart from
        // the windows. That read is used for this poll only, and while it still names the receipt's own hash
        // for a receipt the windows omit, which a backend still on the receipt's branch can answer, the poll
        // waits rather than reading the omission as a contradiction.
        let scan = self.confirmed.scan.as_ref().expect("scan retained");
        let uncovered: BTreeSet<u64> = pending
            .iter()
            .map(|record| record.receipt.block)
            .filter(|block| !scan.hashes.contains_key(block))
            .collect();
        let apart = self.block_hashes(&uncovered).await?;
        let scan = self.confirmed.scan.as_ref().expect("scan retained");
        if pending.iter().any(|record| {
            apart.get(&record.receipt.block) == Some(&record.receipt.block_hash)
                && !scan.canonical.contains(&record.receipt)
        }) {
            return Err(PollError::unsettled(anyhow!(
                "a pending receipt the windows omit is still in its block as a read apart from them names it"
            )));
        }
        let hashes: BTreeMap<u64, B256> = scan.hashes.iter().map(|(n, h)| (*n, *h)).chain(apart).collect();
        let orphans = reconcile(&pending, &scan.canonical, &hashes, to == eligible)?;
        let deferred = orphans.iter().any(|(_, orphaned)| *orphaned == Orphaned::Deferred);
        // From V83 the scan's transitions wait until every orphan is paired with its replacement or, at
        // the eligible head, disproved, and its moves go as one command: a move or a credit landing on a
        // waiting orphan's id is refused. An activation may race this read; whatever it refuses waits for
        // the next poll.
        let together =
            query(tx, |engine| engine.protocol_version() >= ProtocolVersion::V83).await.map_err(PollError::Fatal)?;
        if together {
            if deferred {
                let waiting = orphans.iter().filter(|(_, orphaned)| *orphaned == Orphaned::Deferred).count();
                debug!(
                    waiting,
                    to,
                    eligible,
                    "a reorganisation's orphans wait for the eligible head; its moves and credits wait too"
                );
                return Ok(self.behind(from, eligible));
            }
            self.reorganise(tx, orphans).await?;
            self.apply_confirmed_scan(tx).await?;
        } else {
            let applied = match self.transition_each(tx, orphans).await {
                Ok(()) => self.apply_confirmed_scan(tx).await,
                Err(error) => Err(error),
            };
            raced_activation(tx, applied).await?;
        }
        // An orphan still waiting for its replacement holds the finalised frontier below it. The
        // frontier ends the signing hold a reversal opens (`Accounting::reorganising`), so it follows
        // every transition of the scan.
        if deferred {
            return Ok(self.behind(from, eligible));
        }
        // The frontier moves only to the block the release tag names, or to a scan end the headers link to it,
        // so every read that releases principal is bound to the hash the quorum voted with the tag.
        if let Some(released) = release.tagged.filter(|released| released.number >= from) {
            let end = self.confirmed.scan.as_ref().expect("scan retained").end.expect("a complete window exists");
            match self.frontier(from, end, released).await? {
                Some(finalised) => {
                    if !self.verify_final(from, finalised).await? {
                        return Ok(self.behind(from, eligible));
                    }
                    self.finalise_confirmed_scan(tx, finalised.number, finalised.hash).await?;
                }
                // The scan's end is not on the release tag's chain, and the scan starts again.
                None if self.confirmed.scan.is_none() => return Ok(self.behind(from, eligible)),
                // The scan has yet to come within a block range of the release tag.
                None => {}
            }
        }
        // A carried genesis's stakes still behind the source's frontier read their next window at once.
        let remirroring = self.mirror_staking(tx, release.tagged, release_at).await?;
        let polled = self.finish_confirmed_read(tx, to, eligible).await?;
        Ok(if remirroring { Polled::Read { caught_up: false } } else { polled })
    }

    /// Applies complete canonical receipts after every orphan reversal has been acknowledged.
    /// The per-receipt cursor advances only after acknowledgement; fresh core state handles a
    /// reply lost after application without applying cash again.
    /// Error cases:
    /// - the sequencer stops or refuses a receipt transition
    async fn apply_confirmed_scan(&mut self, tx: &mpsc::Sender<Request>) -> Result<(), PollError> {
        let canonical = &self.confirmed.scan.as_ref().expect("scan retained").canonical;
        let legacy_refs: BTreeSet<_> = canonical.iter().map(|d| format!("{}:{}", d.tx_hash, d.log_index)).collect();
        let prefixes: BTreeSet<_> = canonical.iter().map(|d| format!("{}:", d.tx_hash)).collect();
        let moved_legacy: BTreeSet<_> = query(tx, move |engine| {
            prefixes
                .into_iter()
                .filter(|prefix| engine.unresolved_legacy_references(prefix).any(|r| !legacy_refs.contains(r)))
                .collect()
        })
        .await
        .map_err(PollError::Fatal)?;
        loop {
            let scan = self.confirmed.scan.as_ref().expect("scan retained");
            let Some(receipt) = scan.canonical.get(scan.applied).cloned() else {
                break;
            };
            if !moved_legacy.contains(&format!("{}:", receipt.tx_hash)) {
                let reference = receipt.reference();
                let recorded = query(tx, move |engine| engine.recorded_deposit(&reference).cloned())
                    .await
                    .map_err(PollError::Fatal)?;
                let update = match recorded {
                    Some(recorded) if recorded.status == DepositStatus::Reversed => {
                        DepositUpdate::Reinstate { receipt: receipt.clone(), reversed_at_seq: recorded.updated_at_seq }
                    }
                    Some(_) => {
                        self.confirmed.scan.as_mut().expect("scan retained").applied += 1;
                        continue;
                    }
                    None if self.access_enforced => {
                        let scan = self.confirmed.scan.as_ref().expect("scan retained");
                        let payer = scan.payers.get(&receipt.reference()).copied().flatten();
                        self.sequence_screened(tx, receipt.clone(), payer).await?;
                        self.confirmed.scan.as_mut().expect("scan retained").applied += 1;
                        continue;
                    }
                    None => DepositUpdate::Credit { receipt: receipt.clone() },
                };
                sequence_update(tx, update).await?;
            }
            self.confirmed.scan.as_mut().expect("scan retained").applied += 1;
        }
        Ok(())
    }

    /// Screens a new receipt's beneficiary and vault caller, then sequences it with the evidence;
    /// the engine credits it, leaves it pending, or holds it uncredited.
    /// Error cases:
    /// - the sequencer stops or rejects the receipt
    async fn sequence_screened(
        &self,
        tx: &mpsc::Sender<Request>,
        receipt: DepositReceipt,
        payer: Option<AccountId>,
    ) -> Result<(), PollError> {
        let deposit = receipt_access_deposit(&receipt, payer);
        let request = ScreeningRequest {
            request_hash: Some(deposit_screening_hash(&deposit)),
            account: deposit.account,
            payer,
            peer: None,
        };
        let evidence = screen(self.access.as_deref(), &request).await;
        let span = info_span!("deposit", id = %receipt.reference(), deposit_id = receipt.deposit_id);
        async move {
            let events = submit(tx, Command::AccessCustodyDeposit { receipt, payer, evidence }).await?;
            if events.iter().any(|event| matches!(event, Event::CustodyDepositRejected { .. })) {
                return Err(PollError::inconsistent(anyhow!("sequencer rejected a screened custody receipt")));
            }
            Ok(())
        }
        .instrument(span)
        .await
    }

    /// Releases proved principal without another cash application, then logs the exact frontier.
    /// Error cases:
    /// - the sequencer stops or rejects finality or the complete custody frontier
    async fn finalise_confirmed_scan(
        &mut self,
        tx: &mpsc::Sender<Request>,
        finalised: u64,
        hash: B256,
    ) -> Result<(), PollError> {
        loop {
            let scan = self.confirmed.scan.as_ref().expect("scan retained");
            let Some(receipt) = scan.canonical.get(scan.finalised).filter(|d| d.block <= finalised).cloned() else {
                break;
            };
            let reference = receipt.reference();
            let recorded = query(tx, move |engine| engine.recorded_deposit(&reference).cloned())
                .await
                .map_err(PollError::Fatal)?;
            if let Some(recorded) = recorded.filter(|d| d.status == DepositStatus::Unfinalised) {
                sequence_update(
                    tx,
                    DepositUpdate::Finalise { receipt: receipt.clone(), observed_at_seq: recorded.updated_at_seq },
                )
                .await?;
            }
            self.confirmed.scan.as_mut().expect("scan retained").finalised += 1;
        }
        let count = self.confirmed.scan.as_ref().expect("scan retained").count_at(finalised);
        let checkpoint = CustodyCheckpoint { block: finalised, block_hash: hash, deposit_count: count };
        let events = submit(tx, Command::CustodyFinalised(checkpoint)).await?;
        if events.iter().any(|e| matches!(e, Event::AdminRejected { .. })) {
            return Err(PollError::inconsistent(anyhow!("sequencer refused the finalised custody frontier")));
        }
        Ok(())
    }

    /// Records a poll that stopped short of the eligible block, so a catch-up that never completes
    /// a window, such as one whose reads the head outruns, still raises the lag alert.
    fn behind(&mut self, from: u64, eligible: u64) -> Polled {
        let next_block = self.confirmed.scan.as_ref().map_or(from, |scan| scan.next());
        self.watchdog.observe_read(false, next_block, eligible);
        Polled::Read { caught_up: false }
    }

    /// Records complete read health even when an unchanged canonical scan needs no new range, and
    /// alerts on unresolved inherited principal, resolving the episode once its credit is resolved.
    /// Error cases:
    /// - the sequencer stops while checking unresolved inherited principal
    async fn finish_confirmed_read(
        &mut self,
        tx: &mpsc::Sender<Request>,
        to: u64,
        eligible: u64,
    ) -> Result<Polled, PollError> {
        self.watchdog.observe_read(to == eligible, to.saturating_add(1), eligible);
        if to == eligible {
            let unresolved = query(tx, |engine| engine.unresolved_legacy_deposit().map(|d| d.chain_ref.clone()))
                .await
                .map_err(PollError::Fatal)?;
            if let Some(alerted) = self.legacy_unproven.take().filter(|alerted| unresolved.as_ref() != Some(alerted)) {
                info!(alert_resolved = "legacy_deposit_unproven", chain_ref = %alerted, "legacy principal resolved");
            }
            if let Some(chain_ref) = unresolved {
                error!(alert = "legacy_deposit_unproven", chain_ref = %chain_ref,
                        "legacy principal remains locked: no exact canonical receipt was established");
                self.legacy_unproven = Some(chain_ref);
            }
        }
        Ok(Polled::Read { caught_up: to == eligible })
    }

    /// Reads complete canonical event sets, checking every range's count at its exact end hash.
    /// Consumed headers are discarded.
    /// Error cases:
    /// - missing or inconsistent events, failed reads, or amounts outside the protocol bounds
    pub(super) async fn read_canonical(
        &mut self,
        from: u64,
        to: u64,
        mut next_id: u64,
    ) -> Result<Vec<DepositReceipt>, PollError> {
        let (mut canonical, mut next) = (Vec::new(), from);
        while next <= to {
            let end = to.min(next.saturating_add(self.log_range.get() - 1));
            let window = self.window_hashes(next, end).await?;
            let deposits = self.vault.deposits(next, end).await.map_err(|cause| {
                self.log_range.failed();
                PollError::rpc(cause)
            })?;
            let count =
                self.vault.deposit_count(BlockRef { number: end, hash: window[&end] }).await.map_err(PollError::rpc)?;
            check_complete(&deposits, next, end, next_id, count).map_err(PollError::unsettled)?;
            let mut transaction_ordinals = BTreeMap::<B256, u64>::new();
            for deposit in &deposits {
                if window.get(&deposit.block) != Some(&deposit.block_hash) {
                    return Err(PollError::unsettled(anyhow!("deposit is outside the proven canonical chain")));
                }
                let ordinal = transaction_ordinals.entry(deposit.tx_hash).or_default();
                canonical.push(receipt(deposit, *ordinal)?);
                *ordinal += 1;
            }
            self.chain.clear_window();
            next_id = count;
            next = end.checked_add(1).ok_or_else(|| PollError::inconsistent(anyhow!("block number overflow")))?;
            self.log_range.succeeded();
        }
        Ok(canonical)
    }
}

/// What a pending receipt the canonical scan disproves becomes.
#[derive(Debug, PartialEq, Eq)]
enum Orphaned {
    /// Its transaction-local deposit is canonical again with the same beneficiary and amount.
    Replaced(DepositReceipt),
    /// No replacement can follow, so the full reversal path applies.
    Reversed,
    /// The scan has not reached the eligible head, so a replacement may still appear.
    Deferred,
}

/// Classifies every pending receipt the canonical scan disproves; same-hash absence is inconsistent.
/// A replacement anywhere in the scan pairs with its orphan. An orphan whose transaction is canonical
/// with other economics is reversed; one with no canonical transaction waits for the eligible head.
/// Error cases:
/// - a pending receipt's unchanged canonical block reports a different event set
/// - the scan holds two receipts for one identity or one transaction-local deposit
fn reconcile<'a>(
    pending: &'a [RecordedDeposit],
    canonical: &[DepositReceipt],
    hashes: &BTreeMap<u64, B256>,
    at_head: bool,
) -> Result<Vec<(&'a RecordedDeposit, Orphaned)>, PollError> {
    let exact: BTreeMap<_, _> = canonical.iter().map(|receipt| (receipt.reference(), receipt)).collect();
    let transactions: BTreeMap<_, _> =
        canonical.iter().map(|receipt| (receipt.transaction_reference(), receipt)).collect();
    if exact.len() != canonical.len() || transactions.len() != canonical.len() {
        return Err(PollError::inconsistent(anyhow!("duplicate canonical deposit identity")));
    }
    let mut orphans = Vec::new();
    for recorded in pending {
        let previous = &recorded.receipt;
        if exact.get(&previous.reference()).is_some_and(|receipt| **receipt == *previous) {
            continue;
        }
        let hash = hashes
            .get(&previous.block)
            .ok_or_else(|| PollError::unsettled(anyhow!("canonical proof does not cover a pending receipt")))?;
        if *hash == previous.block_hash {
            return Err(PollError::inconsistent(anyhow!("canonical block omitted or changed a credited receipt")));
        }
        let orphaned = match transactions.get(&previous.transaction_reference()) {
            Some(receipt) if receipt.beneficiary == previous.beneficiary && receipt.amount == previous.amount => {
                Orphaned::Replaced((*receipt).clone())
            }
            Some(_) => Orphaned::Reversed,
            None if at_head => Orphaned::Reversed,
            None => Orphaned::Deferred,
        };
        orphans.push((recorded, orphaned));
    }
    Ok(orphans)
}

/// Converts the chain boundary's base units without rounding or accepting invalid amounts.
/// Error cases:
/// - the token amount is not representable or exceeds the engine input bounds
pub(super) fn receipt(deposit: &VaultDeposit, transaction_deposit_index: u64) -> Result<DepositReceipt, PollError> {
    let amount = usdc_to_fixed(deposit.units)
        .filter(|a| a.is_valid_input())
        .ok_or_else(|| PollError::inconsistent(anyhow!("vault deposit amount is outside protocol bounds")))?;
    Ok(DepositReceipt {
        block: deposit.block,
        block_hash: deposit.block_hash,
        tx_hash: deposit.tx_hash,
        log_index: deposit.log_index,
        transaction_deposit_index,
        deposit_id: deposit.id,
        beneficiary: match deposit.beneficiary {
            Beneficiary::Trader(account) => DepositBeneficiary::Trader { account },
            Beneficiary::House => DepositBeneficiary::House,
        },
        amount,
    })
}

/// Sequences a single receipt transition with the receipt as its correlation identity.
/// Error cases:
/// - the sequencer stops, refuses the stamp, or rejects the transition
async fn sequence_update(tx: &mpsc::Sender<Request>, update: DepositUpdate) -> Result<(), PollError> {
    let span = info_span!("deposit", id = %update.subject().reference(), deposit_id = update.subject().deposit_id);
    async move {
        let (subject, observed) = (update.subject().clone(), observed_revision(&update));
        let events = submit(tx, Command::CustodyDeposit(Box::new(update))).await?;
        if let Some(reason) = refused(&events) {
            let what = "sequencer rejected a custody receipt transition";
            return Err(refusal_stall(tx, &subject, observed, &reason, what).await);
        }
        Ok(())
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests;
