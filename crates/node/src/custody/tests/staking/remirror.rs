//! While a carried genesis's stakes are mirrored again (`V93`), the watcher mirrors only the staking contract's own
//! events: the source's registry, schedule, vote and fault events in those blocks neither mirror nor hold the window
//! for a contract not yet read.

use super::*;
use crate::custody::evm::StakingEventKind;

/// Mirrors `events` over blocks 1 to 3, mirroring again as `remirroring` says, with the registry bound and no
/// schedule, vote or fault proof contract read yet.
async fn mirror_window(
    node: &Node,
    events: &[StakingEvent],
    dir: &Path,
    remirroring: bool,
) -> Result<StakingCursor, PollError> {
    let (reader, mut cursor, mut range) = (StubReader(events.to_vec()), cursor_at(1), AdaptiveSize::new(1_000));
    let path = dir.join("staking-cursor.json");
    let bound = (true, false, false, false);
    mirror(&reader, &node.tx, &mut cursor, &path, &mut range, (1, end_at(3), None), bound, (None, remirroring))
        .await
        .map(|()| cursor)
}

#[tokio::test]
async fn while_the_stakes_are_mirrored_again_only_the_staking_contracts_events_mirror() {
    let dir = scratch("staking-remirror");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([9; 20]);
    let switch = StakingEventKind::RotationChanged { enabled: true, election_cancelled: false };
    let fault = StakingEventKind::FaultProven { member: account, turn: 1, position: 0, signer_epoch: 1, seq: 9 };
    let events = [staked(1, 1, account, 500), event(2, 2, switch), event(3, 3, fault)];

    // Read as ever, the fault holds the window until the fault proof contract is read, and nothing mirrors.
    let held = mirror_window(&node, &events, &dir, false).await.unwrap_err();
    assert!(format!("{held:?}").contains("proven fault"), "{held:?}");
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(0));

    // Mirrored again, the window passes with the stake alone.
    let cursor = mirror_window(&node, &events, &dir, true).await.unwrap();
    assert_eq!(cursor.next_block, 4);
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(500));
    let switched = format!("{}:0", B256::repeat_byte(2));
    assert!(!node.read(move |engine| engine.is_staking_credited(&switched)).await, "the registry event did not mirror");
    node.stop().await;
}
