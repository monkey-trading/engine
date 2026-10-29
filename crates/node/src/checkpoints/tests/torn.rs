//! Conflicts read from one observation: a torn read, as an endpoint answering while it mines a block
//! gives, is retried, and only a conflict a fresh observation repeats stops the worker.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use engine_core::Fixed;
use engine_core::types::AccountId;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::{Chain, poster};
use crate::checkpoints::{CheckpointChain, Conflict, Health, Observation, Poster, VaultState, View, Work};
use crate::log::{Checkpoint, CheckpointParent};
use crate::sequencer::Request;
use crate::test_support::capture_alerts;

/// `Chain` whose reads of a view answer queued states first: each once, or every time when the
/// conflict they show persists.
struct Torn {
    chain: Chain,
    reads: Mutex<VecDeque<(View, VaultState)>>,
    persists: bool,
}

impl CheckpointChain for Torn {
    async fn state(&self, view: View) -> anyhow::Result<VaultState> {
        let queued = {
            let mut reads = self.reads.lock().unwrap();
            let queued = reads.front().filter(|(read, _)| *read == view).map(|(_, state)| state.clone());
            if queued.is_some() && !self.persists {
                reads.pop_front();
            }
            queued
        };
        match queued {
            Some(state) => Ok(state),
            None => self.chain.state(view).await,
        }
    }

    async fn deposited_through(&self, at: &Observation, count: u64) -> anyhow::Result<Fixed> {
        self.chain.deposited_through(at, count).await
    }

    async fn post(&self, cp: &Checkpoint) -> anyhow::Result<()> {
        self.chain.post(cp).await
    }
}

/// The root of checkpoint 4, which the vault holds and checkpoint 5 extends.
fn root_4() -> String {
    format!("0x{}", "04".repeat(32))
}

/// A worker whose queued checkpoint 5 extends checkpoint 4 on the vault, reading `reads` first.
fn extending(reads: impl Fn(&VaultState, &Work) -> Vec<(View, VaultState)>, persists: bool) -> (Poster<Torn>, Work) {
    let (poster, mut work, key) = poster();
    let parent = CheckpointParent { has_checkpoint: true, seq: 4, root: [4; 32].into() };
    let domain = work.checkpoint.domain.unwrap();
    work.checkpoint.parent = Some(parent);
    work.checkpoint.signature = key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, 5, &[3; 32], parent);
    poster.chain.state.lock().unwrap().checkpoint = Some((4, root_4()));
    let vault = poster.chain.state.lock().unwrap().clone();
    let reads = Mutex::new(reads(&vault, &work).into());
    let Poster { chain, registry, domain, signer, retry, pending_deposits_critical_after, post_withheld } = poster;
    let chain = Torn { chain, reads, persists };
    (Poster { chain, registry, domain, signer, retry, pending_deposits_critical_after, post_withheld }, work)
}

/// Each conflict the worker can read from one observation, as the vault state that shows it.
type Case = (&'static str, fn(&VaultState, &Work) -> Vec<(View, VaultState)>);

/// Conflicts one observation can show: the latest view's checkpoint and signer, and the finalized
/// view's checkpoint and signer.
const CASES: [Case; 5] = [
    // #331: the vault's latest seq is read before the post lands and its root after.
    ("a parent torn across the posting block", |vault, work| {
        vec![(View::Latest, VaultState { checkpoint: Some((4, work.checkpoint.root.clone())), ..vault.clone() })]
    }),
    ("another root at the queued seq", |vault, _| {
        vec![(View::Latest, VaultState { checkpoint: Some((5, root_4())), ..vault.clone() })]
    }),
    ("another latest signer", |vault, _| {
        vec![(View::Latest, VaultState { signer: AccountId([9; 20]), ..vault.clone() })]
    }),
    // The first finalized read decides whether to post; the second is the posted checkpoint's.
    ("another finalized root at the posted seq", |vault, _| {
        let torn = VaultState { checkpoint: Some((5, root_4())), ..vault.clone() };
        vec![(View::Finalized, torn.clone()), (View::Finalized, torn)]
    }),
    ("another finalized signer", |vault, _| {
        let torn = VaultState { signer: AccountId([9; 20]), ..vault.clone() };
        vec![(View::Finalized, torn.clone()), (View::Finalized, torn)]
    }),
];

#[tokio::test(start_paused = true)]
async fn a_conflict_read_once_is_retried_and_the_checkpoint_posts() {
    for (name, reads) in CASES {
        let (_guard, mut alerts) = capture_alerts();
        let (poster, work) = extending(reads, false);
        let (work_tx, work_rx) = mpsc::channel(4);
        let (tx, mut status) = mpsc::channel(4);
        work_tx.send(work).await.unwrap();
        let task = tokio::spawn(poster.run(work_rx, tx));
        assert!(
            matches!(status.recv().await, Some(Request::CheckpointStatus { posted: None, health: None, .. })),
            "{name}: the conflicting tick reports nothing"
        );
        assert!(
            matches!(
                status.recv().await,
                Some(Request::CheckpointStatus { posted: Some(5), health: Some(Health::Healthy), .. })
            ),
            "{name}: the next observation posts and reconciles checkpoint 5"
        );
        let raised: Vec<_> =
            std::iter::from_fn(|| alerts.try_recv().ok()).filter_map(|alert| alert.get("alert").cloned()).collect();
        assert_eq!(raised, ["checkpoint_post_failed"], "{name}: an unconfirmed conflict is a Warning");
        drop(work_tx);
        task.await.unwrap().unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_conflict_a_fresh_observation_repeats_stops_the_worker() {
    for (name, reads) in CASES {
        let (_guard, mut alerts) = capture_alerts();
        let (poster, work) = extending(reads, true);
        let (work_tx, work_rx) = mpsc::channel(4);
        let (tx, mut status) = mpsc::channel(4);
        work_tx.send(work).await.unwrap();
        let task = tokio::spawn(poster.run(work_rx, tx));
        // The worker's status channel closes when it stops.
        let drained = timeout(Duration::from_secs(60), async {
            while let Some(request) = status.recv().await {
                assert!(
                    matches!(request, Request::CheckpointStatus { posted: None, health: None, .. }),
                    "{name}: a conflicting tick reports nothing"
                );
            }
        })
        .await;
        assert!(drained.is_ok(), "{name}: the repeated conflict stops the worker");
        assert!(task.await.unwrap().unwrap_err().is::<Conflict>(), "{name}: it stops on the conflict");
        let raised: Vec<_> =
            std::iter::from_fn(|| alerts.try_recv().ok()).filter_map(|alert| alert.get("alert").cloned()).collect();
        assert_eq!(raised, ["checkpoint_post_failed", "checkpoint_conflict"], "{name}: confirmed, it is Critical");
    }
}
