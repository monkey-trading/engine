//! The vote contract's passed proposals mirror only from a vote contract configured over the schedule
//! the registry names, once it is read to record that schedule.

use alloy::primitives::Address;
use engine_core::governance::{GovernanceConfig, command_hash};

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
    node.stop().await;
}

/// A node with staking initialised and the vote contract recorded.
async fn governed_node(dir: &Path) -> Node {
    let node = Node::start_confirmed(dir);
    init_staking(&node).await;
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
