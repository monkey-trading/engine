//! The poster's loop and the rollback check, against a fake chain.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::B256;
use engine_core::types::AccountId;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{Head, Poster, check_rollback};
use crate::log::CheckpointDomain;

mod fake_chain;

use fake_chain::FakeChain;

/// Spawns a poster over `chain` with a one-second retry and the given posting interval.
fn spawn(chain: FakeChain, interval: Duration) -> (watch::Sender<Head>, JoinHandle<anyhow::Result<()>>) {
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    spawn_with(chain, interval, None, sequencer)
}

/// `spawn`, keeping the poster's end of the sequencer channel for the test and seeding the
/// startup-authenticated confirmed head when the test models a restart.
fn spawn_with(
    chain: FakeChain,
    interval: Duration,
    confirmed: Option<(u64, Head)>,
    sequencer: tokio::sync::mpsc::Sender<crate::sequencer::Request>,
) -> (watch::Sender<Head>, JoinHandle<anyhow::Result<()>>) {
    let (tx, rx) = watch::channel(Head::default());
    let poster = Poster {
        chain,
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval,
        retry: Duration::from_secs(1),
        freeze: crate::log_heads::FreezeLatch::default(),
        confirmed,
        anchored: None,
        prefix_ring: Default::default(),
    };
    (tx, tokio::spawn(poster.run(rx, sequencer)))
}

/// The chain access `config/anvil.toml` builds with `[log_heads]`, posting through `endpoint` and
/// keeping its reservation in `dir`; building it touches no network.
pub(super) fn heads_rpc(endpoint: &str, dir: &std::path::Path) -> crate::log_heads::rpc::HeadsRpc {
    let anvil = std::fs::read_to_string(crate::test_support::ANVIL_CONFIG).unwrap();
    let cfg: crate::config::NodeConfig = toml::from_str(&format!(
        "{anvil}\n[checkpoints]\nrpc_url = \"{endpoint}\"\nrelayer = \"0x70997970C51812dc3A010C7d01b50e0d17dc79C8\"\n\
         retry_interval = \"1s\"\nrpc_timeout = \"5s\"\n\n[log_heads]\n\
         contract = \"0x00000000000000000000000000000000000000aa\"\n\
         relayer = \"0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC\"\ninterval = \"1h\"\n"
    ))
    .unwrap();
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let quorum = Arc::new(crate::custody::heads_reader(&cfg.custody).unwrap());
    crate::log_heads::rpc::HeadsRpc::new(
        cfg.checkpoints.as_ref().unwrap(),
        cfg.log_heads.as_ref().unwrap(),
        domain,
        dir,
        quorum,
    )
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn posts_a_moved_head_once_per_interval() {
    let chain = FakeChain::new(None);
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    let head = Head { seq: 1, link: B256::from([1; 32]) };
    tx.send_replace(head);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(chain.posts(), vec![(1, head)]);
    // A confirmed head is not reposted, however many checks run.
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(chain.posts(), vec![(1, head)]);
    // The interval long past, a moved head posts at once...
    let next = Head { seq: 2, link: B256::from([2; 32]) };
    tx.send_replace(next);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(chain.posts(), vec![(1, head), (1, next)]);
    // ...and the next move waits out the interval since that post.
    let third = Head { seq: 3, link: B256::from([3; 32]) };
    tx.send_replace(third);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(chain.posts(), vec![(1, head), (1, next)]);
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(chain.posts(), vec![(1, head), (1, next), (1, third)]);
    drop(tx);
    assert!(task.await.unwrap().is_ok(), "the poster stops cleanly when the sequencer goes away");
}

#[tokio::test(start_paused = true)]
async fn freezes_with_a_refusal_when_the_chain_runs_ahead() {
    let chain = FakeChain::new(Some(Head { seq: 9, link: B256::from([9; 32]) }));
    let (tx, task) = spawn(chain, Duration::from_secs(60));
    tx.send_replace(Head { seq: 3, link: B256::from([3; 32]) });
    let error = task.await.unwrap().unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("overtakes"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn a_rollback_sends_freeze_to_the_sequencer_before_the_poster_stops() {
    let chain = FakeChain::new(Some(Head { seq: 9, link: B256::from([9; 32]) }));
    let (sequencer, mut from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, task) = spawn_with(chain, Duration::from_secs(60), None, sequencer);
    tx.send_replace(Head { seq: 3, link: B256::from([3; 32]) });
    let error = task.await.unwrap().unwrap_err();
    assert!(format!("{error:#}").contains("overtakes"), "{error}");
    assert!(
        matches!(from_poster.recv().await, Some(crate::sequencer::Request::Freeze)),
        "the poster freezes sequencing before the node stops, so the drain stamps nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn a_fork_at_the_same_seq_sends_freeze_before_the_poster_stops() {
    let chain = FakeChain::new(Some(Head { seq: 3, link: B256::from([9; 32]) }));
    let (sequencer, mut from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, task) = spawn_with(chain, Duration::from_secs(60), None, sequencer);
    tx.send_replace(Head { seq: 3, link: B256::from([3; 32]) });
    let error = task.await.unwrap().unwrap_err();
    assert!(format!("{error:#}").contains("foreign"), "{error}");
    assert!(matches!(from_poster.recv().await, Some(crate::sequencer::Request::Freeze)));
}

#[tokio::test(start_paused = true)]
async fn a_fork_at_a_released_seq_freezes_before_the_head_is_signed() {
    // A released head is evidence from its release, not only once signed: a same-key fork's
    // head at a seq this node released but has not posted yet (the interval gate holds it)
    // names another prefix and freezes at once.
    let chain = FakeChain::new(None);
    let (sequencer, mut from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, task) = spawn_with(chain.clone(), Duration::from_secs(60), None, sequencer);
    let first = Head { seq: 1, link: B256::from([1; 32]) };
    tx.send_replace(first);
    tokio::time::sleep(Duration::from_secs(5)).await;
    tx.send_replace(Head { seq: 2, link: B256::from([2; 32]) });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(chain.post_attempts(), 1, "the interval holds the second head unsigned");
    chain.set_posted(Head { seq: 2, link: B256::from([0x22; 32]) });
    let finished = tokio::time::timeout(Duration::from_secs(5), task).await;
    let error = finished.expect("the fork freezes before the interval lets the head post").unwrap().unwrap_err();
    assert!(format!("{error:#}").contains("foreign"), "{error}");
    assert!(matches!(from_poster.recv().await, Some(crate::sequencer::Request::Freeze)));
}

#[tokio::test(start_paused = true)]
async fn releases_between_ticks_cost_no_reads() {
    // A busy sequencer releases many heads inside one retry tick: each is recorded, but the chain
    // is read once per tick, never once per release.
    let chain = FakeChain::new(None);
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let before = chain.reads();
    for seq in 1..=50u64 {
        tx.send_replace(Head { seq, link: B256::left_padding_from(&seq.to_be_bytes()) });
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(chain.reads(), before, "releases between ticks read nothing");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(chain.reads(), before + 1, "the next tick reads once");
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_lost_post_response_still_paces_the_interval() {
    let chain = FakeChain::new(None);
    chain.fail_next_posts(1);
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    tx.send_replace(Head { seq: 1, link: B256::from([1; 32]) });
    // The send's response is lost after the transaction may have broadcast: the attempt paced the
    // gate, so the next read's "no post" does not resend inside the interval.
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(chain.post_attempts(), 1, "a lost response is not retried within the interval");
    tokio::time::sleep(Duration::from_secs(55)).await;
    assert_eq!(chain.post_attempts(), 2, "the interval past, the read-back's absence retries the post");
    assert_eq!(chain.posts(), vec![(1, Head { seq: 1, link: B256::from([1; 32]) })]);
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_conflicting_post_at_the_verified_base_freezes() {
    // Snapshot recovery verified the command-arm link at the base; a conflicting same-seq post
    // that lands afterwards is a fork, compared normally.
    let base = Head { seq: 5, link: B256::from([5; 32]) };
    let chain = FakeChain::new(Some(Head { seq: 5, link: B256::from([9; 32]) }));
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (_tx, rx) = watch::channel(base);
    let poster = Poster {
        chain,
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval: Duration::from_secs(60),
        retry: Duration::from_secs(1),
        freeze: crate::log_heads::FreezeLatch::default(),
        confirmed: None,
        anchored: None,
        prefix_ring: Default::default(),
    };
    let task = tokio::spawn(poster.run(rx, sequencer));
    let error = task.await.unwrap().unwrap_err();
    assert!(format!("{error:#}").contains("foreign"), "{error}");
}

#[tokio::test(start_paused = true)]
async fn a_matching_post_at_the_verified_base_is_confirmed_not_reposted() {
    let base = Head { seq: 5, link: B256::from([5; 32]) };
    let chain = FakeChain::new(Some(base));
    let confirmed = chain.clone();
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, rx) = watch::channel(base);
    let poster = Poster {
        chain,
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval: Duration::from_secs(60),
        retry: Duration::from_secs(1),
        freeze: crate::log_heads::FreezeLatch::default(),
        confirmed: None,
        anchored: None,
        prefix_ring: Default::default(),
    };
    let task = tokio::spawn(poster.run(rx, sequencer));
    tokio::time::sleep(Duration::from_secs(5)).await;
    drop(tx);
    assert!(task.await.unwrap().is_ok(), "a confirmed base head freezes nothing");
    assert!(confirmed.posts().is_empty(), "a confirmed head is not reposted");
}

#[tokio::test(start_paused = true)]
async fn a_read_failure_waits_and_retries() {
    let chain = FakeChain::new(None);
    chain.fail_next_reads(2);
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    let head = Head { seq: 4, link: B256::from([4; 32]) };
    tx.send_replace(head);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(chain.posts(), vec![(1, head)]);
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_signer_rotation_moves_the_signature_to_the_new_epoch() {
    let chain = FakeChain::new(None);
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    tx.send_replace(Head { seq: 1, link: B256::from([1; 32]) });
    tokio::time::sleep(Duration::from_secs(5)).await;
    chain.set_epoch(2);
    tokio::time::sleep(Duration::from_secs(2)).await;
    tx.send_replace(Head { seq: 2, link: B256::from([2; 32]) });
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(
        chain.posts(),
        vec![(1, Head { seq: 1, link: B256::from([1; 32]) }), (2, Head { seq: 2, link: B256::from([2; 32]) })]
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_stalled_anchor_raises_log_head_stalled_once_and_resolves() {
    let held = Head { seq: 1, link: B256::from([1; 32]) };
    let chain = FakeChain::new(Some(held));
    // Every send is lost: the on-chain head stays at seq 1 while this node releases seq 5.
    chain.fail_next_posts(usize::MAX);
    let (_guard, mut router) = crate::alerts::capture_until_dropped();
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, task) = spawn_with(chain.clone(), Duration::from_secs(60), Some((1, held)), sequencer);
    tx.send_replace(Head { seq: 5, link: B256::from([5; 32]) });
    tokio::time::sleep(Duration::from_secs(185)).await;
    let lines = router.take();
    assert_eq!(
        lines.iter().filter(|line| line.name == "log_head_stalled" && line.severity.is_some()).count(),
        1,
        "the stall alert fires once per episode: {lines:?}"
    );
    // The chain head advances past the released head's anchor: resolved, and not raised again.
    chain.set_posted(Head { seq: 5, link: B256::from([5; 32]) });
    tx.send_replace(Head { seq: 6, link: B256::from([6; 32]) });
    tokio::time::sleep(Duration::from_secs(5)).await;
    let lines = router.take();
    assert!(
        lines.iter().any(|line| line.name == "log_head_stalled" && line.severity.is_none()),
        "the advancing head resolves the stall: {lines:?}"
    );
    tokio::time::sleep(Duration::from_secs(65)).await;
    let lines = router.take();
    assert!(
        !lines.iter().any(|line| line.name == "log_head_stalled" && line.severity.is_some()),
        "a resolved episode does not re-alert while posts flow: {lines:?}"
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_waiting_post_is_not_progress_for_the_stall_alarm() {
    let chain = FakeChain::new(Some(Head { seq: 1, link: B256::from([1; 32]) }));
    // The on-chain head never advances and every send answers Waiting: nothing has anchored, and
    // the stall alarm must fire — a wait is not progress.
    chain.wait_posts();
    let (_guard, mut router) = crate::alerts::capture_until_dropped();
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    tx.send_replace(Head { seq: 5, link: B256::from([5; 32]) });
    tokio::time::sleep(Duration::from_secs(185)).await;
    let lines = router.take();
    assert_eq!(
        lines.iter().filter(|line| line.name == "log_head_stalled" && line.severity.is_some()).count(),
        1,
        "a waiting post does not reset the stall timer: {lines:?}"
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_post_extends_the_quorum_confirmed_head_as_its_parent() {
    let chain = FakeChain::new(None);
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    let first = Head { seq: 1, link: B256::from([1; 32]) };
    tx.send_replace(first);
    tokio::time::sleep(Duration::from_secs(5)).await;
    // The epoch's first post names no parent.
    assert_eq!(chain.posts_with_parents(), vec![(1, None, first)]);
    // The quorum confirms the first head; the next post extends it as its parent.
    let second = Head { seq: 2, link: B256::from([2; 32]) };
    tx.send_replace(second);
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(chain.posts_with_parents(), vec![(1, None, first), (1, Some(first), second)]);
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn an_accepted_send_without_inclusion_is_not_anchoring_progress() {
    let held = Head { seq: 1, link: B256::from([1; 32]) };
    let chain = FakeChain::new(Some(held));
    // The endpoint acknowledges every send but the chain head never advances.
    chain.drop_after_accept();
    let (_guard, mut router) = crate::alerts::capture_until_dropped();
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, task) = spawn_with(chain.clone(), Duration::from_secs(60), Some((1, held)), sequencer);
    tx.send_replace(Head { seq: 5, link: B256::from([5; 32]) });
    tokio::time::sleep(Duration::from_secs(185)).await;
    let lines = router.take();
    assert_eq!(
        lines.iter().filter(|line| line.name == "log_head_stalled" && line.severity.is_some()).count(),
        1,
        "acceptance proves no inclusion: the stall alarm fires anyway: {lines:?}"
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_restart_posts_past_the_authenticated_head_naming_it_as_parent() {
    // Restart: the chain holds P, startup authenticated it against the replayed tail, and the
    // recovered log already released H > P. The first post names P as its parent — the contract
    // accepts it — instead of failing NotParent for want of a newer quorum-confirmed head.
    let posted = Head { seq: 3, link: B256::from([3; 32]) };
    let chain = FakeChain::new(Some(posted));
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let head = Head { seq: 7, link: B256::from([7; 32]) };
    let (tx, rx) = watch::channel(head);
    let poster = Poster {
        chain: chain.clone(),
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval: Duration::from_secs(60),
        retry: Duration::from_secs(1),
        freeze: crate::log_heads::FreezeLatch::default(),
        confirmed: Some((1, posted)),
        anchored: None,
        prefix_ring: Default::default(),
    };
    let task = tokio::spawn(poster.run(rx, sequencer));
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(chain.posts_with_parents(), vec![(1, Some(posted), head)]);
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_delayed_inclusion_advances_the_parent_to_the_confirmed_head() {
    // Inclusion lags past the release: H is submitted while the chain holds nothing, H+1 is
    // released and submitted before H lands, then the quorum comes to see H. H is authenticated
    // against what this process submitted, and the next post names it as its parent — the
    // contract accepts it.
    let chain = FakeChain::new(None);
    chain.hold_posts();
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    let first = Head { seq: 1, link: B256::from([1; 32]) };
    tx.send_replace(first);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let second = Head { seq: 2, link: B256::from([2; 32]) };
    tx.send_replace(second);
    tokio::time::sleep(Duration::from_secs(65)).await;
    // Both sends were accepted into the endpoint, but the quorum still sees no head.
    assert_eq!(chain.posts_with_parents(), vec![(1, None, first), (1, None, second)]);
    // Inclusion lands H (the older held-back send): the quorum sees it, and the post after it
    // extends H — not no parent, and not the still-unincluded H+1.
    chain.set_posted(first);
    let third = Head { seq: 3, link: B256::from([3; 32]) };
    tx.send_replace(third);
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(
        chain.posts_with_parents(),
        vec![(1, None, first), (1, None, second), (1, Some(first), third)],
        "the confirmed parent is the head the quorum saw, authenticated against what we submitted"
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn the_stall_resolution_closes_the_firing_episode() {
    // Firing and resolution carry the same fields — the changing seqs live in the message — so
    // the episode router closes the episode the firing opened: one firing page, one resolved
    // page, one key.
    let held = Head { seq: 1, link: B256::from([1; 32]) };
    let chain = FakeChain::new(Some(held));
    chain.fail_next_posts(usize::MAX);
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (_guard, mut router) = crate::alerts::capture_until_dropped();
    let (tx, task) = spawn_with(chain.clone(), Duration::from_secs(60), Some((1, held)), sequencer);
    tx.send_replace(Head { seq: 5, link: B256::from([5; 32]) });
    tokio::time::sleep(Duration::from_secs(185)).await;
    chain.set_posted(Head { seq: 5, link: B256::from([5; 32]) });
    tx.send_replace(Head { seq: 6, link: B256::from([6; 32]) });
    tokio::time::sleep(Duration::from_secs(5)).await;
    drop(tx);
    assert!(task.await.unwrap().is_ok());
    let pages = crate::alerts::pages_for(router.take());
    let stall: Vec<_> = pages.into_iter().filter(|(_, key)| key.starts_with("log_head_stalled")).collect();
    assert_eq!(
        stall,
        vec![
            (crate::alerts::Status::Firing, "log_head_stalled{}".to_string()),
            (crate::alerts::Status::Resolved, "log_head_stalled{}".to_string()),
        ],
        "the resolution ends the episode the firing opened: {stall:?}"
    );
}

mod recovery;

mod rollback;

mod recovery_binding;
mod submission;
