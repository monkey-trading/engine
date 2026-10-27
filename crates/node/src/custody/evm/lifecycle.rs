//! Canonical reconciliation of sequencer-confirmed receipts against Ethereum finality.

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::B256;
use anyhow::anyhow;
use engine_core::access::receipt_access_deposit;
use engine_core::command::Command;
use engine_core::deposits::{
    CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositReject, DepositStatus, DepositUpdate, RecordedDeposit,
};
use engine_core::eip712::deposit_screening_hash;
use engine_core::event::Event;
use engine_core::types::AccountId;
use tokio::sync::mpsc;
use tracing::{Instrument, error, info, info_span};

use crate::access::{ScreeningRequest, screen};
use crate::custody::credit::{query, submit};
use crate::custody::evm::{
    Beneficiary, BlockRef, DepositWatcher, VaultDeposit, VaultIdentity, VaultReader, check_complete,
};
use crate::custody::stall::{PollError, Polled};
use crate::custody::usdc_to_fixed;
use crate::sequencer::Request;

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
            return Ok(Polled::Waiting);
        }
        if !self.prove_confirmed(from, tagged).await? {
            return Ok(self.behind(from, eligible));
        }
        let release_at = if let Some(released) = release.tagged {
            if released.number > tagged.number {
                return Err(PollError::unsettled(anyhow!("withdrawal finality is ahead of the confirmation view")));
            }
            if released.number >= from {
                let Some(hashes) = self.confirmed_window(released.number, released.number).await? else {
                    return Ok(self.behind(from, eligible));
                };
                if hashes[&released.number] != released.hash {
                    return Err(PollError::unsettled(anyhow!("withdrawal finality is on another canonical chain")));
                }
            }
            Some(released.number.min(eligible))
        } else {
            None
        };
        let pending = query(tx, move |engine| engine.unfinalised_deposit_records(from, u64::MAX))
            .await
            .map_err(PollError::Fatal)?;
        self.resume_confirmed_scan(checkpoint, from, next_id)?;
        let needed_hashes: BTreeSet<_> = pending.iter().map(|record| record.receipt.block).collect();
        if !self.advance_confirmed_scan(eligible, &needed_hashes).await? {
            return Ok(self.behind(from, eligible));
        }
        let to = self.confirmed.scan.as_ref().expect("scan retained").end.expect("a complete window exists").number;
        // A lagging quorum tag drops the eligible block below the proven scan end or an already
        // credited receipt; the scan is ahead, so this waits out the tag without arming the lag
        // watchdog, and the poll loop sleeps its normal interval.
        if to > eligible || pending.iter().any(|record| record.receipt.block > eligible) {
            return Ok(Polled::Waiting);
        }
        // Even a replacement moved to an earlier block waits for all inherited pending
        // identities to be covered. An older quorum head proves no absence above itself.
        if pending.iter().any(|record| record.receipt.block > to) {
            return Ok(self.behind(from, eligible));
        }
        for recorded in &pending {
            let block = recorded.receipt.block;
            if !self.confirmed.scan.as_ref().expect("scan retained").hashes.contains_key(&block) {
                let Some(hashes) = self.confirmed_window(block, block).await? else {
                    return Ok(self.behind(from, eligible));
                };
                self.confirmed.scan.as_mut().expect("scan retained").hashes.extend(hashes);
            }
        }
        let scan = self.confirmed.scan.as_ref().expect("scan retained");
        let orphans = reconcile(&pending, &scan.canonical, &scan.hashes, to == eligible)?;
        let deferred = orphans.iter().any(|(_, orphaned)| *orphaned == Orphaned::Deferred);
        // A startup activation may race this poll; the build's command format cannot follow a query.
        let screened_replacements = self.access_enforced;
        for (recorded, orphaned) in orphans {
            let update = match orphaned {
                Orphaned::Replaced(replacement) if screened_replacements => {
                    let payer = self
                        .confirmed
                        .scan
                        .as_ref()
                        .expect("scan retained")
                        .payers
                        .get(&replacement.reference())
                        .copied()
                        .flatten();
                    let update = replacement_update(tx, recorded, replacement.clone()).await?;
                    let deposit = receipt_access_deposit(&replacement, payer);
                    let request = ScreeningRequest {
                        request_hash: deposit_screening_hash(&deposit),
                        account: deposit.account,
                        payer,
                        peer: None,
                    };
                    let evidence = screen(self.access.as_deref(), &request).await;
                    let events =
                        submit(tx, Command::AccessCustodyReplacement { update: Box::new(update), payer, evidence })
                            .await?;
                    if events.iter().any(|event| {
                        matches!(event, Event::CustodyDepositRejected { reason: DepositReject::ProtocolNotActive, .. })
                    }) {
                        return Err(PollError::unsettled(anyhow!(
                            "canonical replacement awaits access protocol activation"
                        )));
                    }
                    if events.iter().any(|event| matches!(event, Event::CustodyDepositRejected { .. })) {
                        return Err(PollError::inconsistent(anyhow!(
                            "sequencer refused screened canonical replacement"
                        )));
                    }
                    continue;
                }
                Orphaned::Replaced(replacement) => replacement_update(tx, recorded, replacement).await?,
                Orphaned::Reversed => DepositUpdate::Reverse {
                    receipt: recorded.receipt.clone(),
                    observed_at_seq: recorded.updated_at_seq,
                },
                Orphaned::Deferred => continue,
            };
            sequence_update(tx, update).await?;
        }
        self.apply_confirmed_scan(tx).await?;
        // An orphan still waiting for its replacement holds the finalised frontier below it.
        if deferred {
            return Ok(self.behind(from, eligible));
        }
        if let Some(finalised) = release_at.filter(|number| *number >= from).map(|number| number.min(to)) {
            let Some(hashes) = self.confirmed_window(finalised, finalised).await? else {
                return Ok(self.behind(from, eligible));
            };
            self.finalise_confirmed_scan(tx, finalised, hashes[&finalised]).await?;
        }
        self.finish_confirmed_read(tx, to, eligible).await
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
            request_hash: deposit_screening_hash(&deposit),
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
    /// a window, such as a header proof outrun by the head, still raises the lag alert.
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

/// Moves an orphan to its canonical replacement in one transition, naming the replacement's own
/// reversal when that exact receipt was credited and reversed before.
/// Error cases:
/// - the sequencer stops, or the replacement is already live beside its orphan
async fn replacement_update(
    tx: &mpsc::Sender<Request>,
    orphan: &RecordedDeposit,
    replacement: DepositReceipt,
) -> Result<DepositUpdate, PollError> {
    let reference = replacement.reference();
    let current =
        query(tx, move |engine| engine.recorded_deposit(&reference).cloned()).await.map_err(PollError::Fatal)?;
    let replacement_reversed_at_seq = match current {
        None => None,
        Some(recorded) if recorded.status == DepositStatus::Reversed => Some(recorded.updated_at_seq),
        Some(_) => return Err(PollError::inconsistent(anyhow!("a replacement receipt is live beside its orphan"))),
    };
    Ok(DepositUpdate::Replace {
        receipt: orphan.receipt.clone(),
        observed_at_seq: orphan.updated_at_seq,
        replacement,
        replacement_reversed_at_seq,
    })
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
        let events = submit(tx, Command::CustodyDeposit(Box::new(update))).await?;
        if events.iter().any(|event| matches!(event, Event::CustodyDepositRejected { .. })) {
            return Err(PollError::inconsistent(anyhow!("sequencer rejected a custody receipt transition")));
        }
        Ok(())
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests;
