//! The worker reports the vault's latest included checkpoint, which runs ahead of the finalized one.

use std::sync::{Arc, Mutex};

use engine_core::types::AccountId;

use super::poster;
use crate::checkpoints::{LatestCheckpoint, Work};
use crate::log::CheckpointParent;
use crate::sequencer::Request;
use crate::tee::EnclaveKey;

/// `count` checkpoints at seqs 5, 6, ..., each signed over its predecessor as the sequencer does.
fn chain_of(count: usize, key: &EnclaveKey, first: &Work) -> Vec<Work> {
    let domain = first.checkpoint.domain.unwrap();
    let mut works = vec![first.clone()];
    for i in 1..count {
        let parent = works.last().unwrap().checkpoint.clone();
        let mut next = first.clone();
        let root = [3 + u8::try_from(i).unwrap(); 32];
        next.checkpoint.seq = parent.seq + 1;
        next.checkpoint.root = format!("0x{}", hex::encode(root));
        let link = CheckpointParent { has_checkpoint: true, seq: parent.seq, root: parent.root.parse().unwrap() };
        next.checkpoint.parent = Some(link);
        next.checkpoint.signature =
            key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, next.checkpoint.seq, &root, link);
        works.push(next);
    }
    works
}

#[tokio::test(start_paused = true)]
async fn the_worker_reports_the_vaults_latest_checkpoint_ahead_of_the_finalized_one() {
    let (mut poster, first, key) = poster();
    // The finalized view stays where it was, so the posted checkpoint is included but not final.
    poster.chain.finalized = Some(Arc::new(Mutex::new(poster.chain.state.lock().unwrap().clone())));
    let works = chain_of(2, &key, &first);
    let (_work_tx, work_rx) = tokio::sync::mpsc::channel(8);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let task = tokio::spawn(poster.run_with_recovery(work_rx, tx, works.clone()));
    let Some(Request::CheckpointStatus { posted, latest, .. }) = rx.recv().await else {
        panic!("the worker reports a status");
    };
    assert_eq!((posted, latest), (None, Some(LatestCheckpoint { seq: works[0].checkpoint.seq, claimable: true })));
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn the_worker_reports_no_claims_on_the_vaults_latest_checkpoint_once_its_signer_is_retired() {
    let (poster, first, key) = poster();
    poster.chain.state.lock().unwrap().checkpoint = Some((first.checkpoint.seq, first.checkpoint.root.clone()));
    poster.chain.state.lock().unwrap().signer = AccountId([0; 20]);
    let works = chain_of(1, &key, &first);
    let (_work_tx, work_rx) = tokio::sync::mpsc::channel(8);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let task = tokio::spawn(poster.run_with_recovery(work_rx, tx, works.clone()));
    let Some(Request::CheckpointStatus { latest, .. }) = rx.recv().await else {
        panic!("the worker reports a status");
    };
    assert_eq!(latest, Some(LatestCheckpoint { seq: works[0].checkpoint.seq, claimable: false }));
    task.abort();
}
