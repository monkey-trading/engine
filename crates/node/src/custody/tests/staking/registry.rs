//! The operator registry's events mirror in chain order with the staking contract's, credited once,
//! and one the engine refuses stalls the mirror with its cursor unmoved.

use super::*;

/// An election of `admin_key` at `epoch`, taking effect at `effective_at` milliseconds.
fn elected(block: u64, tx: u8, epoch: u64, admin_key: AccountId, effective_at: i64) -> StakingEvent {
    event(block, tx, StakingEventKind::Elected { epoch, candidate: AccountId([0xc1; 20]), admin_key, effective_at })
}

#[tokio::test]
async fn registry_events_mirror_with_the_stakes_once_and_a_refused_one_stalls() {
    let dir = scratch("registry-mirror");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let (account, key) = (AccountId([7; 20]), AccountId([0xa1; 20]));
    // An hour past the seven-day floor, so sequencing the election later does not defer it.
    let effective_at = node.read(|engine| engine.now()).await + engine_core::operator_bounds::NOTICE_MS + 3_600_000;
    let switch = StakingEventKind::RotationChanged { enabled: true, election_cancelled: false };
    let reader = StubReader(vec![staked(1, 1, account, 500), event(2, 2, switch), elected(3, 3, 1, key, effective_at)]);
    let mut cursor = cursor_at(1);
    mirror_range(&node, &reader, &mut cursor, &dir, 1, 3).await.unwrap();
    assert_eq!(cursor.next_block, 4);
    let (election, staked_now) = node.read(move |e| (*e.operator_election(), e.staked(account))).await;
    assert!(election.rotation_enabled);
    assert_eq!(election.last.map(|last| (last.epoch, last.admin_key, last.effective_at)), Some((1, key, effective_at)));
    assert_eq!(staked_now, TokenUnits(500));
    assert_eq!(node.read(|e| e.operator_bounds().rotation.map(|r| r.address)).await, Some(key));
    // A re-read of the range credits nothing twice.
    let mut lagging = cursor_at(1);
    mirror_range(&node, &reader, &mut lagging, &dir, 1, 3).await.unwrap();
    assert_eq!(node.read(|e| e.operator_election().last.map(|last| last.epoch)).await, Some(1));

    // An election that does not move forward contradicts the logged registry: the mirror stalls.
    let stale = StubReader(vec![elected(4, 4, 1, key, effective_at)]);
    let error = mirror_range(&node, &stale, &mut cursor, &dir, 4, 4).await.unwrap_err();
    assert!(matches!(error, PollError::Stalled(Stall::Refused(_))), "{error:?}");
    assert_eq!(cursor.next_block, 4, "the cursor holds for the retry");
    node.stop().await;
}
