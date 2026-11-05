//! The submission store's nonce reservations: adoption only on chain evidence, reuse of an
//! unsent nonce, and the pending hand-back the caller bounds in memory.

use alloy::primitives::Address;

use super::Head;
use crate::log_heads::submission::{Reserve, SubmissionStore};

fn deployment() -> (Address, Address, u64) {
    (Address::from([1; 20]), Address::from([2; 20]), 31337)
}

fn store(dir: &std::path::Path) -> SubmissionStore {
    std::fs::create_dir_all(dir).unwrap();
    let (relayer, contract, chain_id) = deployment();
    SubmissionStore::pending(dir, relayer, contract, chain_id).unwrap()
}

/// Stores a reservation as if its send went out.
fn sent(store: &SubmissionStore, reservation: crate::log_heads::submission::Reservation) {
    store.store(&reservation).unwrap();
}

#[test]
fn a_fresh_store_reserves_the_latest_nonce() {
    let dir = crate::test_support::scratch_dir("log-head-submission-fresh");
    let store = store(&dir);
    let head = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    let Reserve::Nonce(reservation) = store.reserve(7, 1, head, 1_000).unwrap() else {
        panic!("a fresh reservation is never a pending decision");
    };
    assert_eq!(reservation.nonce, 7);
}

#[test]
fn a_live_send_for_the_same_head_is_pending_for_the_caller_to_bound() {
    let dir = crate::test_support::scratch_dir("log-head-submission-dedup");
    let store = store(&dir);
    let head = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    let Reserve::Nonce(mut reservation) = store.reserve(7, 1, head, 1_000).unwrap() else { panic!() };
    reservation.tx_hash = Some(alloy::primitives::B256::from([9; 32]));
    sent(&store, reservation);
    // The store hands the pending send back with its hash, whatever the host file claims about
    // time: the caller bounds the wait in memory and reconciles it against the chain.
    let Reserve::Pending(pending) = store.reserve(7, 1, head, u64::MAX).unwrap() else {
        panic!("even a planted future timestamp cannot delay the pending hand-back")
    };
    assert_eq!(pending.tx_hash, Some(alloy::primitives::B256::from([9; 32])));
}

#[test]
fn a_consumed_nonce_adopts_the_new_count_on_chain_evidence() {
    let dir = crate::test_support::scratch_dir("log-head-submission-adopt");
    let store = store(&dir);
    let first = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    assert_eq!(store.reserve(7, 1, first, 1_000).unwrap().nonce(), 7);
    // The chain consumed nonce 7 without a visible head: the count names 8 now.
    let next = Head { seq: 2, link: alloy::primitives::B256::from([2; 32]) };
    let Reserve::Nonce(reservation) = store.reserve(8, 1, next, 1_000).unwrap() else { panic!() };
    assert_eq!(reservation.nonce, 8);
}

#[test]
fn a_crash_before_send_reuses_the_unsent_nonce_for_a_newer_head() {
    let dir = crate::test_support::scratch_dir("log-head-submission-unsent-gap");
    let store = store(&dir);
    let first = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    // The reservation is durable; the send never happened (no hash is stored) — then the
    // process died. The chain count still names the nonce.
    assert_eq!(store.reserve(7, 1, first, 1_000).unwrap().nonce(), 7);
    let next = Head { seq: 2, link: alloy::primitives::B256::from([2; 32]) };
    let Reserve::Nonce(reservation) = store.reserve(7, 1, next, 1_000).unwrap() else {
        panic!("an unsent reservation is rewritten, never a pending decision")
    };
    assert_eq!(reservation.nonce, 7, "no chain evidence consumed the nonce: it is reused, not skipped");
    assert_eq!(reservation.head, Some((1, next)));
    assert!(reservation.tx_hash.is_none());
}

#[test]
fn a_pending_send_at_the_live_nonce_is_handed_back_for_a_newer_head() {
    let dir = crate::test_support::scratch_dir("log-head-submission-pending");
    let store = store(&dir);
    let first = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    let Reserve::Nonce(mut reservation) = store.reserve(7, 1, first, 1_000).unwrap() else { panic!() };
    reservation.tx_hash = Some(alloy::primitives::B256::from([9; 32]));
    sent(&store, reservation);
    // The count still names the nonce the pending send occupies: the caller awaits it inside
    // an in-memory bound, then reconciles it — whatever the newer head or the file's clock.
    let next = Head { seq: 2, link: alloy::primitives::B256::from([2; 32]) };
    let Reserve::Pending(pending) = store.reserve(7, 1, next, u64::MAX).unwrap() else {
        panic!("the pending send is handed back, not replaced")
    };
    assert_eq!(pending.nonce, 7, "the outstanding nonce is kept");
    assert_eq!(pending.head, Some((1, first)));
}

#[test]
fn a_cleared_hash_reuses_the_outstanding_nonce_for_the_newer_head() {
    let dir = crate::test_support::scratch_dir("log-head-submission-reconcile");
    let store = store(&dir);
    let first = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    let Reserve::Nonce(mut reservation) = store.reserve(7, 1, first, 1_000).unwrap() else { panic!() };
    reservation.tx_hash = Some(alloy::primitives::B256::from([9; 32]));
    sent(&store, reservation);
    // A lookup miss cleared the planted hash; the next attempt re-sends at the same nonce.
    let mut cleared = match store.reserve(7, 1, first, u64::MAX).unwrap() {
        Reserve::Pending(mut pending) => {
            pending.tx_hash = None;
            pending.signature = None;
            pending
        }
        other => panic!("expected the pending hand-back, found {other:?}"),
    };
    cleared.stored_at = 2_000;
    store.update(&cleared).unwrap();
    let next = Head { seq: 2, link: alloy::primitives::B256::from([2; 32]) };
    let Reserve::Nonce(rewritten) = store.reserve(7, 1, next, 2_001).unwrap() else {
        panic!("a cleared reservation is rewritten, not awaited")
    };
    assert_eq!(rewritten.nonce, 7, "no chain evidence consumed it: the nonce is reused");
    assert_eq!(rewritten.head, Some((1, next)));
    assert!(rewritten.tx_hash.is_none());
}

#[test]
fn a_reservation_from_another_deployment_is_refused() {
    let dir = crate::test_support::scratch_dir("log-head-submission-mismatch");
    std::fs::create_dir_all(&dir).unwrap();
    let (relayer, contract, chain_id) = deployment();
    let foreign = SubmissionStore::pending(&dir, Address::from([8; 20]), contract, chain_id).unwrap();
    let head = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    let Reserve::Nonce(reservation) = foreign.reserve(7, 1, head, 1_000).unwrap() else { panic!() };
    foreign.store(&reservation).unwrap();
    // The rightful deployment refuses to open storage another relayer, contract, or chain wrote.
    let error = SubmissionStore::pending(&dir, relayer, contract, chain_id).unwrap_err();
    assert!(format!("{error:#}").contains("deployment mismatch"), "{error:#}");
}

#[test]
fn a_stored_nonce_above_the_count_reconciles_down() {
    let dir = crate::test_support::scratch_dir("log-head-submission-reorg");
    let store = store(&dir);
    let first = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    let Reserve::Nonce(mut reservation) = store.reserve(8, 1, first, 1_000).unwrap() else { panic!() };
    reservation.tx_hash = Some(alloy::primitives::B256::from([9; 32]));
    sent(&store, reservation);
    // The count regressed to 7 — a reorg removed the transaction ahead of this nonce, or the host
    // planted a future reservation. The next send takes nonce 7 and fills the lowest outstanding
    // gap, not the stranded 8.
    let next = Head { seq: 2, link: alloy::primitives::B256::from([2; 32]) };
    let Reserve::Nonce(rewritten) = store.reserve(7, 1, next, 2_000).unwrap() else {
        panic!("a nonce above the count reconciles down to a fresh send")
    };
    assert_eq!(rewritten.nonce, 7);
    assert_eq!(rewritten.head, Some((1, next)));
    assert!(rewritten.tx_hash.is_none());
}

#[test]
fn a_stored_nonce_above_the_count_reconciles_down_for_the_same_head_with_a_known_hash() {
    let dir = crate::test_support::scratch_dir("log-head-submission-reorg-same-sent");
    let store = store(&dir);
    let head = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    let Reserve::Nonce(mut reservation) = store.reserve(8, 1, head, 1_000).unwrap() else { panic!() };
    reservation.tx_hash = Some(alloy::primitives::B256::from([9; 32]));
    sent(&store, reservation);
    // The count regressed to 7 while the endpoint may still hold the transaction at 8: the
    // same-head pending dedup must not strand the nonce above the count — the send reconciles
    // down to 7 and fills the gap.
    let Reserve::Nonce(rewritten) = store.reserve(7, 1, head, 2_000).unwrap() else {
        panic!("a stored nonce above the count reconciles down, whatever head it names")
    };
    assert_eq!(rewritten.nonce, 7);
    assert_eq!(rewritten.head, Some((1, head)));
    assert!(rewritten.tx_hash.is_none());
}

#[test]
fn a_stored_nonce_above_the_count_reconciles_down_for_the_same_head_unsent() {
    let dir = crate::test_support::scratch_dir("log-head-submission-reorg-same-unsent");
    let store = store(&dir);
    let head = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    // The reservation is durable; the send never happened. The count regressed to 7: the next
    // send takes 7, not the stranded 8.
    assert_eq!(store.reserve(8, 1, head, 1_000).unwrap().nonce(), 8);
    let Reserve::Nonce(rewritten) = store.reserve(7, 1, head, 2_000).unwrap() else {
        panic!("a stored nonce above the count reconciles down, sent or not")
    };
    assert_eq!(rewritten.nonce, 7);
    assert_eq!(rewritten.head, Some((1, head)));
    assert!(rewritten.tx_hash.is_none());
}

#[tokio::test]
async fn a_timed_out_write_keeps_its_lock_until_it_completes() {
    // The caller's timeout abandons the wait, never the lock: while a stalled writer holds it, a
    // retry's write waits rather than overlapping it, and the committed record decodes as one
    // whole write, whichever order the waiting writers wake in.
    let dir = crate::test_support::scratch_dir("log-head-submission-lock-queue");
    let store = store(&dir);
    let first = Head { seq: 1, link: alloy::primitives::B256::from([1; 32]) };
    let second = Head { seq: 2, link: alloy::primitives::B256::from([2; 32]) };
    let mut reservation = match store.reserve(7, 1, first, 1_000).unwrap() {
        Reserve::Nonce(reservation) => reservation,
        other => panic!("a fresh store reserves a nonce: {other:?}"),
    };
    reservation.tx_hash = Some(alloy::primitives::B256::from([8; 32]));
    // The test holds the lock as a stalled writer would; both updates wait behind it.
    let held = store.wait_lock().unwrap();
    let writers: Vec<_> = [first, second]
        .into_iter()
        .map(|head| {
            let (store, mut write) = (store.clone(), reservation.clone());
            write.head = Some((1, head));
            tokio::task::spawn_blocking(move || store.update(&write))
        })
        .collect();
    // Time for both writers to reach the lock; nothing below depends on which reached it first.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(writers.iter().all(|writer| !writer.is_finished()), "no write completes while the lock is held");
    drop(held);
    for writer in writers {
        writer.await.unwrap().expect("each waiting writer completes once the lock frees");
    }
    let loaded = store.read().unwrap().expect("the committed record decodes");
    assert!(
        [Some((1, first)), Some((1, second))].contains(&loaded.head),
        "the record is one whole write: {:?}",
        loaded.head
    );
    drop(store.lock().expect("the lock is free once every writer finished"));
}
