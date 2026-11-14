//! The operator registry's events mirror in chain order with the staking contract's, credited once,
//! and one the engine refuses stalls the mirror with its cursor unmoved. The schedule's events mirror
//! only from the schedule both the registry and the config name, once it is read to record the
//! registry.

use alloy::primitives::Address;
use engine_core::turns::SetMember;

use super::*;

/// The schedule the fake chain's registry names.
const SCHEDULE: Address = Address::repeat_byte(0x5c);

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

#[tokio::test]
async fn an_installation_mirrors_while_rotation_is_off() {
    let dir = scratch("registry-install");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let installed = AccountId([0xa2; 20]);
    let install = StakingEventKind::Installed { candidate: AccountId([0xc2; 20]), admin_key: installed };
    let reader = StubReader(vec![event(2, 2, install)]);
    let mut cursor = cursor_at(1);
    mirror_range(&node, &reader, &mut cursor, &dir, 1, 2).await.unwrap();
    assert_eq!(cursor.next_block, 3);
    let (recorded, rotation) =
        node.read(move |e| (e.operator_election().installed.map(|i| i.admin_key), e.operator_bounds().rotation)).await;
    assert_eq!(recorded, Some(installed));
    assert_eq!(rotation.map(|rotation| rotation.address), Some(installed));
    node.stop().await;
}

#[tokio::test]
async fn a_staking_contract_another_registry_deployed_stalls_the_poll_before_any_mirror() {
    let dir = scratch("registry-binding");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([3; 20]);
    let chains = poll_chains(10);
    for chain in &chains {
        chain.0.lock().unwrap().staking.push(staked(2, 1, account, 500));
    }
    let toml = format!(
        r#"{}
        staking = {{ contract = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a", cursor_path = "{}", registry = {{ contract = "0x7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c7c" }} }}
        "#,
        evm_toml(&dir, 100),
        dir.join("staking-cursor.json").display()
    );
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    let mut watcher = DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap();
    let error = watcher.poll(&node.tx).await.unwrap_err();
    let PollError::Stalled(Stall::Inconsistent(cause)) = error else { panic!("{error:?}") };
    assert!(cause.to_string().contains("records registry"), "{cause}");
    assert!(node.read(move |engine| engine.staked_balances().is_empty()).await, "nothing mirrors");
    // The configured registry passes, once.
    let mut watcher = staking_watcher(&chains, &dir);
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(500));
    node.stop().await;
}

#[tokio::test]
async fn a_staking_contract_deployed_after_the_first_window_scans_up_to_it_and_then_binds() {
    let dir = scratch("registry-binding-late-deploy");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([4; 20]);
    let chains = poll_chains(10);
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.staking_deployed_at = Some(5);
        state.staking.push(staked(6, 1, account, 500));
    }
    let mut watcher = staking_watcher_with_range(&chains, &dir, 4);
    // The first window ends before the deployment: no binding read can answer, and the scan
    // advances instead of trapping on the same range. Window proofs advance per poll, so poll
    // until the mirror credits the event; a poll that errors, as the trap did, fails the test.
    for _ in 0..10 {
        if node.read(move |engine| engine.staked(account)).await == TokenUnits(500) {
            break;
        }
        watcher.poll(&node.tx).await.unwrap();
    }
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(500));
    let cursor: StakingCursor =
        serde_json::from_str(&std::fs::read_to_string(dir.join("staking-cursor.json")).unwrap()).unwrap();
    assert_eq!(cursor.next_block, 8);
    node.stop().await;
}

#[tokio::test]
async fn events_stall_until_the_registry_binding_is_verified() {
    let dir = scratch("registry-binding-events");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([5; 20]);
    let reader = StubReader(vec![staked(2, 1, account, 500)]);
    let mut cursor = cursor_at(1);
    // A poll that has not verified the registry binding read no staking code at the window's
    // end, so events there cannot be legitimate: they stall the mirror instead of applying.
    let error = mirror(
        &reader,
        &node.tx,
        &mut cursor,
        &dir.join("staking-cursor.json"),
        &mut AdaptiveSize::new(1_000),
        1..=3,
        &hashes_through(3),
        (false, true, true),
        None,
    )
    .await
    .unwrap_err();
    let PollError::Stalled(Stall::Inconsistent(cause)) = error else { panic!("{error:?}") };
    assert!(cause.to_string().contains("no code"), "{cause}");
    assert_eq!(cursor, cursor_at(1), "the cursor holds for the retry");
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(0));
    // Once the binding is verified the same window mirrors.
    mirror(
        &reader,
        &node.tx,
        &mut cursor,
        &dir.join("staking-cursor.json"),
        &mut AdaptiveSize::new(1_000),
        1..=3,
        &hashes_through(3),
        (true, true, true),
        None,
    )
    .await
    .unwrap();
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(500));
    node.stop().await;
}

#[tokio::test]
async fn a_registry_naming_a_schedule_the_config_does_not_stalls_the_poll_before_any_mirror() {
    let dir = scratch("schedule-named");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([6; 20]);
    let chains = schedule_chains(SCHEDULE, Some(REGISTRY), account);
    for (what, configured) in [("another schedule", Some(Address::repeat_byte(0x5d))), ("no schedule", None)] {
        let error = schedule_watcher(&chains, &dir, configured).poll(&node.tx).await.unwrap_err();
        let PollError::Stalled(Stall::Inconsistent(cause)) = error else { panic!("{what}: {error:?}") };
        assert!(cause.to_string().contains(&format!("the registry names schedule {SCHEDULE}")), "{what}: {cause}");
        assert_eq!(mirrored(&node, account).await, (TokenUnits(0), vec![]), "{what}: an event mirrored");
    }
    node.stop().await;
}

#[tokio::test]
async fn a_schedule_recording_another_registry_stalls_the_poll_before_any_mirror() {
    let dir = scratch("schedule-records-registry");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let (account, other) = (AccountId([7; 20]), Address::repeat_byte(0x6c));
    let chains = schedule_chains(SCHEDULE, Some(other), account);
    let error = schedule_watcher(&chains, &dir, Some(SCHEDULE)).poll(&node.tx).await.unwrap_err();
    let PollError::Stalled(Stall::Inconsistent(cause)) = error else { panic!("{error:?}") };
    assert!(cause.to_string().contains(&format!("the schedule {SCHEDULE} records registry {other}")), "{cause}");
    assert_eq!(mirrored(&node, account).await, (TokenUnits(0), vec![]), "an event mirrored");
    node.stop().await;
}

#[tokio::test]
async fn a_schedule_event_before_the_schedule_is_read_to_name_the_registry_stalls_the_poll() {
    let dir = scratch("schedule-unbound-event");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([8; 20]);
    // The schedule has no code at the window's end, so no read has bound it, yet the window names
    // its rotation set: a contradiction, not an event to mirror.
    let chains = schedule_chains(SCHEDULE, None, account);
    let error = schedule_watcher(&chains, &dir, Some(SCHEDULE)).poll(&node.tx).await.unwrap_err();
    let PollError::Stalled(Stall::Inconsistent(cause)) = error else { panic!("{error:?}") };
    assert!(cause.to_string().contains("the schedule has not been read to name the registry"), "{cause}");
    assert_eq!(mirrored(&node, account).await, (TokenUnits(0), vec![]), "an event mirrored");
    node.stop().await;
}

#[tokio::test]
async fn the_schedule_both_name_that_records_the_registry_mirrors_its_events() {
    let dir = scratch("schedule-bound");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([9; 20]);
    let chains = schedule_chains(SCHEDULE, Some(REGISTRY), account);
    schedule_watcher(&chains, &dir, Some(SCHEDULE)).poll(&node.tx).await.unwrap();
    assert_eq!(mirrored(&node, account).await, (TokenUnits(500), vec![1]));
    node.stop().await;
}

/// The schedule's adoption of rotation set 1, of one member.
fn adopted(block: u64, tx: u8) -> StakingEvent {
    let members =
        vec![SetMember { candidate: AccountId([0xc1; 20]), admin_key: AccountId([0xa1; 20]), weight: TokenUnits(1) }];
    let effective_at = 1_793_000_000_000;
    event(block, tx, StakingEventKind::RotationSetAdopted { set: 1, epoch: 1, effective_at, quorum: true, members })
}

/// Chains whose registry names `named`, whose schedule records `registry`, and whose window holds a
/// stake by `account` and the schedule's adoption of a rotation set.
fn schedule_chains(named: Address, registry: Option<Address>, account: AccountId) -> [FakeChain; 3] {
    let chains = poll_chains(10);
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        (state.schedule.named, state.schedule.registry) = (named, registry);
        state.staking.extend([staked(2, 1, account, 500), adopted(3, 2)]);
    }
    chains
}

/// The confirmed watcher over `chains`, configured with the fake chain's registry and `schedule`.
fn schedule_watcher(
    chains: &[FakeChain; 3],
    dir: &Path,
    schedule: Option<Address>,
) -> DepositWatcher<Quorum<FakeChain>> {
    let schedule = schedule.map_or_else(String::new, |schedule| format!(r#", schedule = "{schedule}""#));
    let toml = format!(
        r#"{}
        staking = {{ contract = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a", cursor_path = "{}", registry = {{ contract = "{REGISTRY}"{schedule} }} }}
        "#,
        evm_toml(dir, 100),
        dir.join("staking-cursor.json").display()
    );
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap()
}

/// What `account` has staked in the engine, and the rotation sets it has mirrored.
async fn mirrored(node: &Node, account: AccountId) -> (TokenUnits, Vec<u32>) {
    node.read(move |engine| (engine.staked(account), engine.daily_turns().sets.keys().copied().collect())).await
}
