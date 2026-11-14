//! Recovery behaviors of the poster's loop: heads that land despite a lost response or across
//! a restart, the stall alarm carried across a restart from the checked head or the finalized
//! checkpoint, same-epoch reorg reparenting, an emptied epoch, the transport-failure alert
//! episodes' resolutions, and adoption bound to this log's prefixes — a foreign prefix freezes, a
//! head with no retained evidence is not adopted.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::B256;
use engine_core::types::AccountId;
use tokio::sync::watch;

use super::fake_chain::FakeChain;
use super::{spawn, spawn_with};
use crate::log::CheckpointDomain;
use crate::log_heads::{Head, Poster};

#[tokio::test(start_paused = true)]
async fn a_head_lands_despite_its_lost_response_and_parents_the_next_post() {
    // The send's response is lost after the transaction may have broadcast: the head is an
    // anchor from signing, so when it lands after a newer head released, the read adopts it and
    // the newer head's post names it — the contract accepts it.
    let chain = FakeChain::new(None);
    chain.fail_next_posts(1);
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    let first = Head { seq: 1, link: B256::from([1; 32]) };
    tx.send_replace(first);
    tokio::time::sleep(Duration::from_secs(5)).await;
    // Inclusion lands before the newer release posts.
    chain.set_posted(first);
    let second = Head { seq: 2, link: B256::from([2; 32]) };
    tx.send_replace(second);
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(
        chain.posts_with_parents(),
        vec![(1, Some(first), second)],
        "the landed head is the confirmed parent, not no parent: {:?}",
        chain.posts_with_parents()
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_signed_head_stays_adoptable_after_the_released_ring_evicts_its_seq() {
    // The send's response is lost, and more than the ring's capacity of newer heads release
    // before the signed head lands, as a busy sequencer does inside one posting interval: the
    // ring no longer holds its seq, but a head this process signed is an anchor, so the read
    // adopts it and the next post extends it.
    let chain = FakeChain::new(None);
    chain.fail_next_posts(1);
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    let signed = Head { seq: 1, link: B256::from([1; 32]) };
    tx.send_replace(signed);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(chain.post_attempts(), 1, "the head was signed and sent");
    let last = crate::log::PrefixRing::CAPACITY as u64 + 2;
    for seq in 2..=last {
        tx.send_replace(Head { seq, link: B256::left_padding_from(&seq.to_be_bytes()) });
        // Each release wakes the poster, which records it before the next one.
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    chain.set_posted(signed);
    tokio::time::sleep(Duration::from_secs(65)).await;
    let tip = Head { seq: last, link: B256::left_padding_from(&last.to_be_bytes()) };
    assert_eq!(
        chain.posts_with_parents(),
        vec![(1, Some(signed), tip)],
        "the signed head outlives the ring as the parent: {:?}",
        chain.posts_with_parents()
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_head_signed_before_a_crash_is_adopted_after_restart() {
    // The first process signs and broadcasts H, loses the response, and crashes; the second
    // starts with the replay-seeded prefix ring (the replay authenticated H's cumulative prefix)
    // and adopts H when it lands — the next post extends it and the contract accepts it.
    let first = Head { seq: 1, link: B256::from([1; 32]) };
    let chain = FakeChain::new(None);
    chain.fail_next_posts(1);
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (tx1, rx1) = watch::channel(first);
    let poster = Poster {
        chain: chain.clone(),
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval: Duration::from_secs(60),
        retry: Duration::from_secs(1),
        freeze: crate::log_heads::FreezeLatch::default(),
        confirmed: None,
        anchored: None,
        trusted: None,
        prefix_ring: Default::default(),
        stall_intervals: 3,
        heartbeat: None,
    };
    let task1 = tokio::spawn(poster.run(rx1, sequencer));
    tx1.send_replace(first);
    tokio::time::sleep(Duration::from_secs(5)).await;
    // The broadcast is lost to this process (the response never arrived); the node crashes.
    task1.abort();
    // Restart: the recovered log extends past H, and the replay seeds the ring with H's prefix.
    let released = Head { seq: 2, link: B256::from([2; 32]) };
    let (tx, rx) = watch::channel(released);
    let mut prefix_ring = crate::log::PrefixRing::default();
    prefix_ring.record(first.seq, first.link);
    let poster = Poster {
        chain: chain.clone(),
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval: Duration::from_secs(60),
        retry: Duration::from_secs(1),
        freeze: crate::log_heads::FreezeLatch::default(),
        confirmed: None,
        anchored: None,
        trusted: None,
        prefix_ring,
        stall_intervals: 3,
        heartbeat: None,
    };
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let task = tokio::spawn(poster.run(rx, sequencer));
    // H's transaction lands after the restart.
    chain.set_posted(first);
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(
        chain.posts_with_parents(),
        vec![(1, Some(first), released)],
        "the replay-seeded prefix adopts the head the crashed process signed: {:?}",
        chain.posts_with_parents()
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_restart_remembers_the_authenticated_startup_head() {
    // The startup-authenticated head stays adoptable even where the ring carries no evidence
    // for it (here: an empty ring): post and confirm Q, then a reorg restores P — the poster
    // reparents to P and the next post names it.
    let held = Head { seq: 1, link: B256::from([1; 32]) };
    let chain = FakeChain::new(Some(held));
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, task) = spawn_with(chain.clone(), Duration::from_secs(60), Some((1, held)), sequencer);
    let removed = Head { seq: 2, link: B256::from([2; 32]) };
    tx.send_replace(removed);
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(chain.posts_with_parents(), vec![(1, Some(held), removed)]);
    // The reorg removes Q and restores the startup head P.
    chain.set_posted(held);
    let surviving = Head { seq: 3, link: B256::from([3; 32]) };
    tx.send_replace(surviving);
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(
        chain.posts_with_parents(),
        vec![(1, Some(held), removed), (1, Some(held), surviving)],
        "the startup-authenticated head is the surviving parent after a reorg: {:?}",
        chain.posts_with_parents()
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_restart_counts_trusted_time_since_the_checked_head_in_the_stall_alarm() {
    // Startup checked the posted head at seq 1, stamped four posting intervals ago in trusted
    // time — a restart, or a log cut back to that head, cannot move either. Every post is lost:
    // once the first has had a posting interval to be included, the stall fires, though this
    // process has seen no advance and its own clock has run for one interval only.
    let held = Head { seq: 1, link: B256::from([1; 32]) };
    let chain = FakeChain::new(Some(held));
    chain.fail_next_posts(usize::MAX);
    let (_guard, mut router) = crate::alerts::capture_until_dropped();
    let (tx, task) = restarted(chain, Some(held));
    tokio::time::sleep(Duration::from_secs(55)).await;
    assert!(!router.take().iter().any(|line| line.name == "log_head_stalled"), "the first post's interval runs first");
    tokio::time::sleep(Duration::from_secs(10)).await;
    let lines = router.take();
    assert_eq!(
        lines.iter().filter(|line| line.name == "log_head_stalled" && line.severity.is_some()).count(),
        1,
        "the restart neither counts the checked head as an advance nor resets the time since it: {lines:?}"
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_restart_after_a_long_stop_gives_its_first_post_an_interval_to_land() {
    // The same restart, with the first post's inclusion slow: it lands ten retry ticks after the
    // send, well inside the posting interval, and nothing pages meanwhile.
    let held = Head { seq: 1, link: B256::from([1; 32]) };
    let chain = FakeChain::new(Some(held));
    chain.hold_posts();
    let (_guard, mut router) = crate::alerts::capture_until_dropped();
    let (tx, task) = restarted(chain.clone(), Some(held));
    tokio::time::sleep(Duration::from_secs(10)).await;
    let sent = Head { seq: 5, link: B256::from([5; 32]) };
    assert_eq!(chain.posts_with_parents(), vec![(1, Some(held), sent)]);
    chain.set_posted(sent);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let lines = router.take();
    assert!(!lines.iter().any(|line| line.name == "log_head_stalled"), "{lines:?}");
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_restart_with_no_head_posted_counts_from_the_finalized_checkpoint() {
    // The poster's side of a seq-0 anchor (recovery's is in `recovery_binding` and the paired
    // tests): no head of this key is posted, and startup anchored on the vault-finalized checkpoint,
    // stamped four intervals ago. Every post is lost: nothing pages while the first post has its
    // interval, then the stall fires, though a restart reset this process's own clock.
    let chain = FakeChain::new(None);
    chain.fail_next_posts(usize::MAX);
    let (_guard, mut router) = crate::alerts::capture_until_dropped();
    let (tx, task) = restarted(chain, None);
    tokio::time::sleep(Duration::from_secs(55)).await;
    assert!(!router.take().iter().any(|line| line.name == "log_head_stalled"), "the first post's interval runs first");
    tokio::time::sleep(Duration::from_secs(10)).await;
    let lines = router.take();
    assert_eq!(
        lines.iter().filter(|line| line.name == "log_head_stalled" && line.severity.is_some()).count(),
        1,
        "the finalized checkpoint's stamp survives the restart: {lines:?}"
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_same_epoch_reorg_reparents_to_the_surviving_authenticated_head() {
    // Reads are at `latest`: a reorg removes the confirmed head H and restores P. P is an
    // authenticated ancestor — it was signed and posted by this node — so the next post names P,
    // and the replacement submission records it as the parent.
    let chain = FakeChain::new(None);
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    let parent = Head { seq: 1, link: B256::from([1; 32]) };
    tx.send_replace(parent);
    tokio::time::sleep(Duration::from_secs(5)).await;
    let removed = Head { seq: 2, link: B256::from([2; 32]) };
    tx.send_replace(removed);
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(chain.posts_with_parents(), vec![(1, None, parent), (1, Some(parent), removed)]);
    // The reorg removes H and restores P; the endpoint dropped H's transaction.
    chain.set_posted(parent);
    let surviving = Head { seq: 3, link: B256::from([3; 32]) };
    tx.send_replace(surviving);
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(
        chain.posts_with_parents(),
        vec![(1, None, parent), (1, Some(parent), removed), (1, Some(parent), surviving)],
        "the surviving authenticated head is the parent again, not the reorged-out one: {:?}",
        chain.posts_with_parents()
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_reorg_that_empties_the_epoch_posts_the_next_head_parentless() {
    // The reorg removes even the epoch's first post: the epoch reads empty, the confirmed parent
    // resets, and the next post goes parentless under the first-post rule.
    let chain = FakeChain::new(None);
    let (tx, task) = spawn(chain.clone(), Duration::from_secs(60));
    let first = Head { seq: 1, link: B256::from([1; 32]) };
    tx.send_replace(first);
    tokio::time::sleep(Duration::from_secs(5)).await;
    chain.clear_posted();
    let next = Head { seq: 3, link: B256::from([3; 32]) };
    tx.send_replace(next);
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert_eq!(
        chain.posts_with_parents(),
        vec![(1, None, first), (1, None, next)],
        "the emptied epoch's next post names no parent: {:?}",
        chain.posts_with_parents()
    );
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test(start_paused = true)]
async fn the_read_failure_episode_resolves_when_reads_recover() {
    let held = Head { seq: 1, link: B256::from([1; 32]) };
    let chain = FakeChain::new(Some(held));
    chain.fail_next_reads(2);
    let (_guard, mut router) = crate::alerts::capture_until_dropped();
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, task) = spawn_with(chain.clone(), Duration::from_secs(60), Some((1, held)), sequencer);
    tx.send_replace(Head { seq: 5, link: B256::from([5; 32]) });
    tokio::time::sleep(Duration::from_secs(10)).await;
    let mut seen = router.take();
    assert_eq!(
        seen.iter().filter(|line| line.name == "log_head_read_failed" && line.severity.is_some()).count(),
        2,
        "each failed read warns: {seen:?}"
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    drop(tx);
    assert!(task.await.unwrap().is_ok());
    seen.extend(router.take());
    let pages = crate::alerts::pages_for(seen);
    let episode: Vec<_> = pages.into_iter().filter(|(_, key)| key.starts_with("log_head_read_failed")).collect();
    assert_eq!(
        episode,
        vec![
            (crate::alerts::Status::Firing, "log_head_read_failed{}".to_string()),
            (crate::alerts::Status::Resolved, "log_head_read_failed{}".to_string()),
        ],
        "the successful read resolves the firing episode: {episode:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_post_failure_episode_resolves_when_posts_recover() {
    let held = Head { seq: 1, link: B256::from([1; 32]) };
    let chain = FakeChain::new(Some(held));
    chain.fail_next_posts(1);
    let (_guard, mut router) = crate::alerts::capture_until_dropped();
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, task) = spawn_with(chain.clone(), Duration::from_secs(60), Some((1, held)), sequencer);
    tx.send_replace(Head { seq: 5, link: B256::from([5; 32]) });
    tokio::time::sleep(Duration::from_secs(5)).await;
    let mut seen = router.take();
    assert!(
        seen.iter().any(|line| line.name == "log_head_post_failed" && line.severity.is_some()),
        "the failed post warns: {seen:?}"
    );
    tokio::time::sleep(Duration::from_secs(65)).await;
    drop(tx);
    assert!(task.await.unwrap().is_ok());
    seen.extend(router.take());
    let pages = crate::alerts::pages_for(seen);
    let episode: Vec<_> = pages.into_iter().filter(|(_, key)| key.starts_with("log_head_post_failed")).collect();
    assert_eq!(
        episode,
        vec![
            (crate::alerts::Status::Firing, "log_head_post_failed{}".to_string()),
            (crate::alerts::Status::Resolved, "log_head_post_failed{}".to_string()),
        ],
        "the accepted post resolves the firing episode: {episode:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_foreign_head_at_a_retained_seq_freezes() {
    // A same-key fork's head at a retained seq carries a foreign cumulative prefix: the
    // signature would verify, but the prefix check binds adoption to this log — freeze.
    let ours = Head { seq: 5, link: B256::from([5; 32]) };
    let chain = FakeChain::new(None);
    let (sequencer, mut from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, rx) = watch::channel(ours);
    let mut prefix_ring = crate::log::PrefixRing::default();
    prefix_ring.record(ours.seq, ours.link);
    let freeze = crate::log_heads::FreezeLatch::default();
    let poster = Poster {
        chain: chain.clone(),
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval: Duration::from_secs(60),
        retry: Duration::from_secs(1),
        freeze: freeze.clone(),
        confirmed: None,
        anchored: None,
        trusted: None,
        prefix_ring,
        stall_intervals: 3,
        heartbeat: None,
    };
    let task = tokio::spawn(poster.run(rx, sequencer));
    tokio::time::sleep(Duration::from_secs(3)).await;
    chain.set_posted(Head { seq: 5, link: B256::from([9; 32]) });
    let finished = tokio::time::timeout(Duration::from_secs(5), task).await;
    let error = finished.expect("the poster freezes and stops").unwrap().unwrap_err();
    assert!(format!("{error:#}").contains("foreign"), "{error}");
    assert!(freeze.frozen(), "a foreign head freezes sequencing");
    assert!(matches!(from_poster.recv().await, Some(crate::sequencer::Request::Freeze)));
    drop(tx);
}

#[tokio::test(start_paused = true)]
async fn a_divergent_history_head_freezes_at_the_ancestor() {
    // A discarded tail restarted divergently: the divergent copy's head at a lower retained
    // seq names a prefix this log does not hold — freeze, not adoption.
    let tip = Head { seq: 10, link: B256::from([10; 32]) };
    let chain = FakeChain::new(None);
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, rx) = watch::channel(tip);
    let mut prefix_ring = crate::log::PrefixRing::default();
    for seq in 3..=10u64 {
        prefix_ring.record(seq, B256::from([seq as u8; 32]));
    }
    let freeze = crate::log_heads::FreezeLatch::default();
    let poster = Poster {
        chain: chain.clone(),
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval: Duration::from_secs(60),
        retry: Duration::from_secs(1),
        freeze: freeze.clone(),
        confirmed: None,
        anchored: None,
        trusted: None,
        prefix_ring,
        stall_intervals: 3,
        heartbeat: None,
    };
    let task = tokio::spawn(poster.run(rx, sequencer));
    tokio::time::sleep(Duration::from_secs(3)).await;
    // The divergent copy's ancestor at seq 4: same key, same seq, another prefix.
    chain.set_posted(Head { seq: 4, link: B256::from([0x44; 32]) });
    let finished = tokio::time::timeout(Duration::from_secs(5), task).await;
    let error = finished.expect("the poster freezes and stops").unwrap().unwrap_err();
    assert!(format!("{error:#}").contains("foreign"), "{error}");
    assert!(freeze.frozen());
    drop(tx);
}

#[tokio::test(start_paused = true)]
async fn a_head_beyond_the_retained_evidence_is_not_adopted() {
    // An old head older than the ring and not the startup head carries no evidence either
    // way: the poster keeps its parent (the safe stall) — no adoption, no freeze — and the
    // stale-parent send fails the contract's chaining rule.
    let tip = Head { seq: 10, link: B256::from([10; 32]) };
    let chain = FakeChain::new(None);
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, rx) = watch::channel(tip);
    let mut prefix_ring = crate::log::PrefixRing::default();
    prefix_ring.record(tip.seq, tip.link);
    let freeze = crate::log_heads::FreezeLatch::default();
    let poster = Poster {
        chain: chain.clone(),
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval: Duration::from_secs(60),
        retry: Duration::from_secs(1),
        freeze: freeze.clone(),
        confirmed: None,
        anchored: None,
        trusted: None,
        prefix_ring,
        stall_intervals: 3,
        heartbeat: None,
    };
    let task = tokio::spawn(poster.run(rx, sequencer));
    tokio::time::sleep(Duration::from_secs(3)).await;
    // The empty epoch's first post lands immediately and confirms the tip.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(chain.posts(), vec![(1, tip)], "the first post of the empty epoch is parentless");
    // An ancestor older than the ring floor lands: no evidence, so no adoption and no freeze.
    // The stale parent fails the contract's chaining rule: no further post lands.
    chain.set_posted(Head { seq: 7, link: B256::from([7; 32]) });
    tokio::time::sleep(Duration::from_secs(65)).await;
    assert!(!freeze.frozen(), "absent evidence never freezes");
    assert_eq!(chain.posts(), vec![(1, tip)], "the stale parent fails the chaining rule: no further post lands");
    assert_eq!(chain.post_attempts(), 1, "a head never posts extending itself: the safe stall signs nothing");
    drop(tx);
    assert!(task.await.unwrap().is_ok());
}

// ── Helpers ──

/// A poster restarted on a log released to seq 5, whose startup checked `held` at seq 1, or, with
/// no head posted, anchored on the vault-finalized checkpoint, stamped four posting intervals before
/// its trusted time.
fn restarted(
    chain: FakeChain,
    held: Option<Head>,
) -> (watch::Sender<Head>, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let (tx, rx) = watch::channel(Head { seq: 5, link: B256::from([5; 32]) });
    let interval = Duration::from_secs(60);
    let clock: Arc<dyn crate::clock::Clock> = Arc::new(crate::clock::test_clock::TestClock::at(1_000_000_000));
    let samples = watch::channel(Some(crate::test_support::sample_around(clock.as_ref()))).1;
    let stamped = clock.now_ms() - i64::try_from((interval * 4).as_millis()).unwrap();
    let poster = Poster {
        chain,
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval,
        retry: Duration::from_secs(1),
        freeze: crate::log_heads::FreezeLatch::default(),
        confirmed: held.map(|held| (1, held)),
        anchored: Some((held.map_or(0, |held| held.seq), Some(stamped))),
        trusted: Some(crate::trusted_time::TrustedClock::new(clock, samples, Duration::from_secs(1))),
        prefix_ring: Default::default(),
        stall_intervals: 3,
        heartbeat: None,
    };
    (tx, tokio::spawn(poster.run(rx, sequencer)))
}
