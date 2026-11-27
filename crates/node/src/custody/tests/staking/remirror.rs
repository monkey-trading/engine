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

/// The engine's refusal of a frontier while the stakes are mirrored again is the re-mirror's own: it pages at once as
/// a contradiction, where the same refusal otherwise waits as a stamp the node's time may admit.
#[tokio::test]
async fn a_refused_frontier_while_the_stakes_are_mirrored_again_pages_at_once() {
    let dir = scratch("staking-remirror-refused");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([9; 20]);
    mirror_window(&node, &[staked(1, 1, account, 500)], &dir, true).await.unwrap();
    // A window ending before the frontier the engine completed is refused.
    let behind = |remirroring| {
        let (reader, mut cursor, mut range) = (StubReader(vec![]), cursor_at(1), AdaptiveSize::new(1_000));
        let (path, node) = (dir.join("staking-cursor-behind.json"), &node);
        async move {
            let bound = (true, false, false, false);
            let window = (1, end_at(2), None);
            mirror(&reader, &node.tx, &mut cursor, &path, &mut range, window, bound, (None, remirroring)).await
        }
    };
    let refused = behind(true).await.expect_err("the frontier is behind the one completed");
    assert!(matches!(refused, PollError::Stalled(Stall::Inconsistent(_))), "{refused:?}");
    assert!(format!("{refused:?}").contains("mirrored again"), "{refused:?}");
    let waits = behind(false).await.expect_err("the same refusal outside a re-mirror");
    assert!(matches!(waits, PollError::Stalled(Stall::Refused(_))), "{waits:?}");
    node.stop().await;
}

/// The reference source's carry, applied to `node` after its `InitStaking`: its stakes are left to mirror again, to
/// the source's frontier, whose block this returns.
async fn carried_node(node: &Node) -> u64 {
    let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v90-carried.jsonl"));
    let file = crate::carry::build(path, None).unwrap();
    init_staking(node).await;
    for part in file.parts {
        let events = node.submit(Command::Admin(AdminCommand::CarryState(part))).await;
        assert_eq!(events.iter().filter(|e| matches!(e, Event::AdminRejected { .. })).count(), 0, "{events:?}");
    }
    node.read(|engine| engine.staking().and_then(|staking| staking.remirror).map(|pending| pending.through.block))
        .await
        .expect("the carry leaves the stakes to be mirrored again")
}

/// While a carried genesis's stakes are behind the source's frontier, each poll reads one window and the next
/// follows at once, not after the poll interval: the poll says it has not caught up, until the window that
/// reaches the frontier.
#[tokio::test]
async fn while_behind_the_sources_frontier_each_poll_asks_for_the_next_window_at_once() {
    let dir = scratch("remirror-pace");
    let node = Node::start_confirmed(&dir);
    let through = carried_node(&node).await;
    assert!(through >= 4, "the reference source's frontier is block {through}");
    let (range, chains) = (through / 4, poll_chains(through + 10));
    let toml = format!(
        r#"{}
        staking = {{ contract = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a", cursor_path = "{}", deployment_block = 0, registry = {{ contract = "{REGISTRY}" }} }}
        "#,
        // The deposit scan starts at the head, so it is caught up at once and only the staking mirror is behind.
        evm_toml(&dir, range).replace("start_block = 0", &format!("start_block = {}", through + 10)),
        dir.join("staking-cursor.json").display()
    );
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    let mut watcher = DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap();
    let mut read = 0;
    // The windows below the frontier. The one that reaches it ends the re-mirror, and this stub chain's block there
    // is not the source's.
    for window in 1..=through.div_ceil(range) - 1 {
        let polled = watcher.poll(&node.tx).await.unwrap();
        assert!(!polled.caught_up(), "window {window}: the poll waits its interval with the frontier still ahead");
        read = node.read(|engine| engine.staking().and_then(|staking| staking.completed_through)).await.unwrap().block;
        assert_eq!(read, window * range - 1, "window {window} is one range of blocks");
    }
    assert!(read < through);
    node.stop().await;
}
