//! The vote contract's passed proposals mirror only from a vote contract configured over the schedule
//! the registry names, once it is read to record that schedule; from `V59` the registry's turn state,
//! read at the frontier once the registry is bound, reaches the engine with it.

use alloy::primitives::Address;
use alloy::sol_types::SolCall;
use engine_core::governance::{GovernanceConfig, command_hash};
use engine_core::protocol::ProtocolVersion;

use super::*;

const SCHEDULE: Address = Address::repeat_byte(0x5c);
const VOTE: Address = Address::repeat_byte(0x5f);

#[tokio::test]
async fn a_vote_contract_recording_another_schedule_stalls_the_poll_before_any_mirror() {
    let dir = scratch("vote-records-schedule");
    let node = governed_node(&dir).await;
    let other = Address::repeat_byte(0x6d);
    let error = vote_watcher(&vote_chains(Some(other)), &dir).poll(&node.tx).await.unwrap_err();
    let PollError::Stalled(Stall::Inconsistent(cause)) = error else { panic!("{error:?}") };
    assert!(cause.to_string().contains(&format!("the vote contract {VOTE} records schedule {other}")), "{cause}");
    assert!(!node.read(|e| e.is_staking_credited(&passed_ref())).await, "a proposal mirrored");
    node.stop().await;
}

#[tokio::test]
async fn a_vote_event_before_the_vote_contract_is_read_to_name_the_schedule_stalls_the_poll() {
    let dir = scratch("vote-unbound-event");
    let node = governed_node(&dir).await;
    // The vote contract has no code at the window's end, so no read has bound it, yet the window
    // names its passed proposal: a contradiction, not an event to mirror.
    let error = vote_watcher(&vote_chains(None), &dir).poll(&node.tx).await.unwrap_err();
    let PollError::Stalled(Stall::Inconsistent(cause)) = error else { panic!("{error:?}") };
    assert!(cause.to_string().contains("the vote contract has not been read to name the schedule"), "{cause}");
    assert!(!node.read(|e| e.is_staking_credited(&passed_ref())).await, "a proposal mirrored");
    node.stop().await;
}

#[tokio::test]
async fn the_vote_contract_over_the_configured_schedule_mirrors_its_passed_proposals() {
    let dir = scratch("vote-bound");
    let node = governed_node(&dir).await;
    vote_watcher(&vote_chains(Some(SCHEDULE)), &dir).poll(&node.tx).await.unwrap();
    // Credited once; with rotation off the engine refuses what it carries.
    assert!(node.read(|e| e.is_staking_credited(&passed_ref())).await);
    assert!(node.read(|e| e.governance().restrictions.is_empty()).await);
    // The frontier carries the registry's state: no turn names the operator.
    assert_eq!(progress_turns(&dir), [Some(None)]);
    node.stop().await;
}

#[tokio::test]
async fn the_turn_the_registry_names_the_operator_by_reaches_the_engine_with_the_frontier() {
    let dir = scratch("vote-named-turn");
    let node = governed_node(&dir).await;
    let chains = vote_chains(Some(SCHEDULE));
    // Blocks are twelve seconds apart: the turn started with block 1, before the window's end.
    for chain in &chains {
        chain.0.lock().unwrap().schedule.named_turn_until = Some(86_400 + 12);
    }
    vote_watcher(&chains, &dir).poll(&node.tx).await.unwrap();
    assert_eq!(node.read(|e| e.governance().turns_from).await, Some(12_000));
    // Read at the frontier the engine records, and only there.
    let frontier = node.read(|e| e.staking().and_then(|staking| staking.completed_through)).await.expect("progress");
    let reads = chains[0].0.lock().unwrap().schedule.turn_reads.clone();
    assert_eq!(reads, [BlockRef { number: frontier.block, hash: frontier.block_hash }]);
    node.stop().await;
}

#[tokio::test]
async fn before_v59_or_without_the_registry_bound_no_frontier_carries_the_registrys_turn() {
    // A node at `V56`: the frontier advances, with no read and no turn.
    let dir = scratch("vote-named-turn-v56");
    let node = governed_node_at(&dir, ProtocolVersion::V56).await;
    let chains = vote_chains(Some(SCHEDULE));
    chains.iter().for_each(|chain| chain.0.lock().unwrap().schedule.named_turn_until = Some(86_400 + 12));
    vote_watcher(&chains, &dir).poll(&node.tx).await.unwrap();
    assert!(node.read(|e| e.staking().and_then(|staking| staking.completed_through)).await.is_some());
    assert!(chains[0].0.lock().unwrap().schedule.turn_reads.is_empty());
    assert_eq!(progress_turns(&dir), [None]);
    node.stop().await;
    // A window ending before the staking contract's deployment binds no registry: no read either.
    let dir = scratch("vote-named-turn-unbound");
    let node = governed_node(&dir).await;
    let chains = poll_chains(10);
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.staking_deployed_at = Some(1_000);
        state.schedule.vote = Some((VOTE, None));
        state.schedule.named_turn_until = Some(86_400 + 12);
    }
    vote_watcher(&chains, &dir).poll(&node.tx).await.unwrap();
    assert!(chains[0].0.lock().unwrap().schedule.turn_reads.is_empty());
    assert_eq!(progress_turns(&dir), [None]);
    node.stop().await;
}

#[test]
fn the_registrys_rotation_state_decodes_as_the_named_turns_boundary() {
    use crate::custody::registry::{decode_named_turn, rotationStateCall, rotationStateReturn};
    let answer = |generation, until| -> alloy::primitives::Bytes {
        let state = rotationStateReturn { enabled: true, release: B256::ZERO, generation, until };
        rotationStateCall::abi_encode_returns(&state).into()
    };
    assert_eq!(decode_named_turn(&answer(0, 86_400 + 60)).unwrap(), None, "no turn names the operator");
    assert_eq!(decode_named_turn(&answer(3, 86_400 + 60)).unwrap(), Some(60_000));
    assert!(decode_named_turn(&answer(3, 60)).is_err(), "a turn that ends within its first day");
    assert!(decode_named_turn(&alloy::primitives::Bytes::from_static(b"no")).is_err());
}

/// The registry turn state each staking frontier in the log in `dir` carried, in order.
fn progress_turns(dir: &Path) -> Vec<Option<Option<engine_core::types::Millis>>> {
    crate::test_support::log_entries(&dir.join("log"))
        .into_iter()
        .filter_map(|entry| match entry {
            crate::log::LogEntry::Command(entry) => match entry.sequenced.cmd {
                Command::StakingProgress { named_turn, .. } => Some(named_turn),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// A node with staking initialised and the vote contract recorded.
async fn governed_node(dir: &Path) -> Node {
    let node = Node::start_confirmed(dir);
    init_staking(&node).await;
    let config = GovernanceConfig { vote: AccountId(VOTE.0.0), restriction_ttl_ms: 86_400_000 };
    assert_eq!(node.submit(Command::Admin(AdminCommand::SetGovernance(config))).await, [Event::AdminApplied]);
    node
}

/// `governed_node` under `version` rather than the latest.
async fn governed_node_at(dir: &Path, version: ProtocolVersion) -> Node {
    let node = Node::start_confirmed(dir);
    let activation = Command::Admin(AdminCommand::ActivateProtocol { version });
    assert_eq!(node.submit(activation).await, [Event::AdminApplied]);
    let start = crate::test_support::trusted_now().latest_ms + 60_000;
    let config =
        StakingConfig { liquidity_floor: Fixed::ONE, epoch_ms: 30 * 60_000, start_ms: start, cooldown_ms: 86_400_000 };
    assert_eq!(node.submit(Command::Admin(AdminCommand::InitStaking(config))).await, [Event::AdminApplied]);
    let config = GovernanceConfig { vote: AccountId(VOTE.0.0), restriction_ttl_ms: 86_400_000 };
    assert_eq!(node.submit(Command::Admin(AdminCommand::SetGovernance(config))).await, [Event::AdminApplied]);
    node
}

/// The passed proposal's event: a fast-path reduce-only restriction.
fn passed() -> StakingEvent {
    let command = AdminCommand::SetReduceOnly { market: None, reduce_only: true };
    let kind = StakingEventKind::ProposalPassed {
        proposal: 1,
        fast: true,
        command_hash: command_hash(&command),
        command: Some(Box::new(command)),
    };
    event(3, 2, kind)
}

fn passed_ref() -> String {
    passed().chain_ref()
}

/// Chains whose registry names the schedule, which records the registry, and whose vote contract
/// records `schedule`, with a window holding the passed proposal.
fn vote_chains(schedule: Option<Address>) -> [FakeChain; 3] {
    let chains = poll_chains(10);
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.schedule.named = SCHEDULE;
        state.schedule.registry = Some(REGISTRY);
        state.schedule.vote = Some((VOTE, schedule));
        state.staking.push(passed());
    }
    chains
}

/// The confirmed watcher over `chains`, configured with the fake chain's registry, the schedule and
/// the vote contract.
fn vote_watcher(chains: &[FakeChain; 3], dir: &Path) -> DepositWatcher<Quorum<FakeChain>> {
    let toml = format!(
        r#"{}
        staking = {{ contract = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a", cursor_path = "{}", registry = {{ contract = "{REGISTRY}", schedule = "{SCHEDULE}", vote = {{ contract = "{VOTE}", restriction_ttl = "1d" }} }} }}
        "#,
        evm_toml(dir, 100),
        dir.join("staking-cursor.json").display()
    );
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap()
}
