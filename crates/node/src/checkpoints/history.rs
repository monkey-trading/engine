//! Accepted checkpoint history from the log's signed-parent chain and the vault's signer epochs.

use std::collections::BTreeSet;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;

use crate::checkpoints::{AcceptedCheckpoint, CheckpointChain, Observation, SignerHistory, signature};
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent};

/// Authenticates every signed-parent checkpoint up to the finalized anchor with one vault read for
/// the anchor and one per signer epoch, not per checkpoint. The vault accepts a checkpoint only as
/// the child of its latest one and only under its signer at the time, so an unbroken signed-parent
/// chain that ends at the vault's own anchor is its accepted history, each checkpoint signed by an
/// epoch signer. `candidates` are those checkpoints in log order after `from`, the chain's start: none
/// for the log's start, or a daily snapshot's checkpoint, which a start reads no log before, whose
/// checkpoints' signers through `from` the snapshot carries as `prior`, so a start from it knows the earlier
/// signers a replay from the log's start does; `payout_epochs` are the epochs typed payouts name, and `earlier`
/// the seqs before `from` of the checkpoints they name, which the vault gives in one read.
/// Error cases:
/// - the chain breaks or misses the anchor, a signature fails, a signer has no epoch, or a custody
///   read fails
/// - the vault accepted no checkpoint at a seq of `earlier`
pub(crate) async fn accepted_history(
    chain: &impl CheckpointChain,
    at: &Observation,
    domain: CheckpointDomain,
    (anchor, from, prior): (CheckpointParent, CheckpointParent, &BTreeSet<AccountId>),
    candidates: &[Checkpoint],
    (payout_epochs, earlier): (BTreeSet<u64>, BTreeSet<u64>),
) -> anyhow::Result<SignerHistory> {
    let evidence = at.lifecycle.as_ref().context("signer history requires canonical lifecycle evidence")?;
    let mut tip = from;
    let mut historical = prior.iter().any(|signer| *signer != evidence.signer);
    for checkpoint in candidates {
        ensure!(checkpoint.parent == Some(tip), "checkpoint at seq {} breaks the signed parent chain", checkpoint.seq);
        signature::verify_domain(checkpoint, domain)?;
        historical |= checkpoint.signer.parse::<AccountId>()? != evidence.signer;
        tip = CheckpointParent { has_checkpoint: true, seq: checkpoint.seq, root: checkpoint.root.parse()? };
    }
    ensure!(tip == anchor, "signed parent chain does not end at the finalized vault anchor");
    let mut epochs = payout_epochs;
    let anchor_authority = match candidates.last() {
        Some(last) => {
            let accepted = chain.accepted_checkpoints(at, &[last.seq]).await?;
            let authority = accepted.first().context("vault lacks its finalized anchor")?.clone();
            authority.verify(last)?;
            epochs.insert(authority.signer_epoch);
            Some(authority)
        }
        None => None,
    };
    // The checkpoints before the chain's start that payouts after it name, as the vault accepted them.
    let earlier: Vec<u64> = earlier.into_iter().collect();
    let accepted_earlier = match earlier.is_empty() {
        true => Vec::new(),
        false => chain.accepted_checkpoints(at, &earlier).await?,
    };
    for seq in &earlier {
        ensure!(
            accepted_earlier.iter().any(|accepted| accepted.seq == *seq),
            "the vault accepted no checkpoint at seq {seq}, which a typed payout names"
        );
    }
    epochs.extend(accepted_earlier.iter().map(|accepted| accepted.signer_epoch));
    // Which epoch a retired signer held is unknown until every epoch is read.
    if historical {
        epochs.extend(1..=evidence.signer_epoch);
    }
    let mut history = SignerHistory::default();
    history.authenticate_epochs(chain, at, epochs).await?;
    for accepted in &accepted_earlier {
        ensure!(
            history.epochs.get(&accepted.signer_epoch) == Some(&accepted.signer),
            "checkpoint signer differs from immutable epoch history"
        );
    }
    history.checkpoints.extend(accepted_earlier);
    if let Some(authority) = anchor_authority {
        ensure!(
            history.epochs.get(&authority.signer_epoch) == Some(&authority.signer),
            "checkpoint signer differs from immutable epoch history"
        );
    }
    for checkpoint in candidates {
        let signer = checkpoint.signer.parse::<AccountId>()?;
        let signer_epoch = history
            .epochs
            .iter()
            .rev()
            .find_map(|(epoch, named)| (*named == signer).then_some(*epoch))
            .context("checkpoint signer is absent from the vault's signer history")?;
        history.checkpoints.push(AcceptedCheckpoint {
            seq: checkpoint.seq,
            root: checkpoint.root.clone(),
            signer,
            signer_epoch,
        });
    }
    Ok(history)
}
