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
/// epoch signer. `candidates` are those checkpoints in log order; `payout_epochs` are the epochs
/// typed payouts name.
/// Error cases:
/// - the chain breaks or misses the anchor, a signature fails, a signer has no epoch, or a custody
///   read fails
pub(crate) async fn accepted_history(
    chain: &impl CheckpointChain,
    at: &Observation,
    domain: CheckpointDomain,
    anchor: CheckpointParent,
    candidates: &[Checkpoint],
    payout_epochs: BTreeSet<u64>,
) -> anyhow::Result<SignerHistory> {
    let evidence = at.lifecycle.as_ref().context("signer history requires canonical lifecycle evidence")?;
    let mut tip = CheckpointParent::default();
    let mut historical = false;
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
    // Which epoch a retired signer held is unknown until every epoch is read.
    if historical {
        epochs.extend(1..=evidence.signer_epoch);
    }
    let mut history = SignerHistory::default();
    history.authenticate_epochs(chain, at, epochs).await?;
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
