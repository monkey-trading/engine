//! The fault proof contract's proven faults mirror only from a contract configured over the custody
//! vault, once it is read to record that vault, and, once the schedule's governance record names a
//! fault proof contract, only from that one.

use alloy::primitives::Address;
use engine_core::operator_faults::{FaultProofConfig, Penalty};

use super::*;
use crate::custody::evm::GovernanceRecord;
use crate::test_support::capture_alerts_and_resolutions;

const SCHEDULE: Address = Address::repeat_byte(0x5c);
const FAULT_PROOF: Address = Address::repeat_byte(0x5e);
const MEMBER: AccountId = AccountId([0xb2; 20]);

#[tokio::test]
async fn a_fault_proof_contract_recording_another_vault_stalls_the_poll_before_any_mirror() {
    let dir = scratch("fault-proof-records-vault");
    let node = penalising_node(&dir).await;
    let other = Address::repeat_byte(0x6d);
    let error = fault_watcher(&fault_chains(Some(other)), &dir).poll(&node.tx).await.unwrap_err();
    let PollError::Stalled(Stall::Inconsistent(cause)) = error else { panic!("{error:?}") };
    let expected = format!("the fault proof contract {FAULT_PROOF} records vault {other}, not the configured {VAULT}");
    assert!(cause.to_string().contains(&expected), "{cause}");
    assert!(!node.read(|e| e.is_staking_credited(&proven_ref())).await, "a fault mirrored");
    node.stop().await;
}

#[tokio::test]
async fn a_fault_proof_contract_the_schedules_record_does_not_name_stalls_the_poll_before_any_mirror() {
    let dir = scratch("fault-proof-not-recorded");
    let node = penalising_node(&dir).await;
    // The configured contract answers that it proves faults over the vault, as a lookalike would, but
    // the schedule's record names the timelock's fault proof contract.
    let chains = fault_chains(Some(VAULT));
    let recorded = Address::repeat_byte(0x6f);
    record(&chains, GovernanceRecord::Set { vote: Address::repeat_byte(0x5f), fault_proof: recorded });
    let error = fault_watcher(&chains, &dir).poll(&node.tx).await.unwrap_err();
    let PollError::Stalled(Stall::Inconsistent(cause)) = error else { panic!("{error:?}") };
    let expected = format!(
        "the schedule's governance record names fault proof contract {recorded}, not the configured {FAULT_PROOF}"
    );
    assert!(cause.to_string().contains(&expected), "{cause}");
    assert!(!node.read(|e| e.is_staking_credited(&proven_ref())).await, "a fault mirrored");
    assert!(node.read(|e| e.operator_faults().haircuts.is_empty()).await, "a haircut started");
    node.stop().await;
}

#[tokio::test]
async fn the_fault_proof_contract_the_schedules_record_names_mirrors_its_proven_faults() {
    let dir = scratch("fault-proof-recorded");
    let node = penalising_node(&dir).await;
    let chains = fault_chains(Some(VAULT));
    record(&chains, GovernanceRecord::Set { vote: Address::repeat_byte(0x5f), fault_proof: FAULT_PROOF });
    fault_watcher(&chains, &dir).poll(&node.tx).await.unwrap();
    assert!(node.read(|e| e.is_staking_credited(&proven_ref())).await);
    node.stop().await;
}

#[tokio::test]
async fn a_fault_before_the_contract_is_read_to_name_the_vault_stalls_the_poll() {
    let dir = scratch("fault-proof-unbound-event");
    let node = penalising_node(&dir).await;
    // The contract has no code at the window's end, so no read has bound it, yet the window names its
    // proven fault: a contradiction, not an event to mirror.
    let error = fault_watcher(&fault_chains(None), &dir).poll(&node.tx).await.unwrap_err();
    let PollError::Stalled(Stall::Inconsistent(cause)) = error else { panic!("{error:?}") };
    assert!(cause.to_string().contains("the fault proof contract has not been read to name the vault"), "{cause}");
    assert!(!node.read(|e| e.is_staking_credited(&proven_ref())).await, "a fault mirrored");
    node.stop().await;
}

#[tokio::test]
async fn the_fault_proof_contract_over_the_vault_mirrors_its_proven_faults_once() {
    let dir = scratch("fault-proof-bound");
    let node = penalising_node(&dir).await;
    let chains = fault_chains(Some(VAULT));
    let mut watcher = fault_watcher(&chains, &dir);
    watcher.poll(&node.tx).await.unwrap();
    assert!(node.read(|e| e.is_staking_credited(&proven_ref())).await);
    let haircuts = node.read(|e| e.operator_faults().haircuts.clone()).await;
    assert_eq!(haircuts.keys().collect::<Vec<_>>(), [&MEMBER]);
    // A later poll over the same window mirrors nothing twice: the haircut does not restart.
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.read(|e| e.operator_faults().haircuts.clone()).await, haircuts);
    node.stop().await;
}

#[tokio::test]
async fn a_recorded_fault_proof_contract_with_no_code_pages_over_a_window_with_no_event() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let dir = scratch("fault-proof-undeployed-recorded");
    let node = penalising_node(&dir).await;
    let mut watcher = fault_watcher(&quiet_chains(None), &dir);
    watcher.poll(&node.tx).await.unwrap();
    let alert = alerts.try_recv().expect("the undeployed contract pages");
    assert_eq!(alert["alert"], "fault_proof_undeployed");
    assert_eq!(alert["contract"], FAULT_PROOF.to_string());
    assert_eq!(alert["level"], "WARN");
    assert!(resolved.try_recv().is_err(), "resolved with no code");
    node.stop().await;
}

#[tokio::test]
async fn a_fault_proof_contract_the_log_does_not_record_pages_nobody_before_its_deployment() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let dir = scratch("fault-proof-undeployed-unrecorded");
    // Staking is initialised, but no `SetFaultProof` has applied: a catch-up over blocks before
    // the deployment is no fault of the address.
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    fault_watcher(&quiet_chains(None), &dir).poll(&node.tx).await.unwrap();
    assert!(alerts.try_recv().is_err(), "paged before the record");
    assert!(resolved.try_recv().is_err());
    node.stop().await;
}

#[tokio::test]
async fn the_page_resolves_once_the_contract_binds_to_the_vault() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let dir = scratch("fault-proof-undeployed-resolves");
    let node = penalising_node(&dir).await;
    let chains = quiet_chains(None);
    let mut watcher = fault_watcher(&chains, &dir);
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(alerts.try_recv().expect("paged")["alert"], "fault_proof_undeployed");
    // The contract is deployed over the vault, and the chain moves on to a later window.
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.schedule.fault_proof = Some((FAULT_PROOF, Some(VAULT)));
        state.head = 12;
        state.final_block = Some(12);
        state.withdrawal_block = Some(12);
    }
    watcher.poll(&node.tx).await.unwrap();
    assert!(alerts.try_recv().is_err(), "paged again once bound");
    let done = resolved.try_recv().expect("resolved");
    assert_eq!(done["alert_resolved"], "fault_proof_undeployed");
    assert_eq!(done["contract"], FAULT_PROOF.to_string());
    // Bound for good: a later window reads nothing more.
    watcher.poll(&node.tx).await.unwrap();
    assert!(resolved.try_recv().is_err(), "resolved twice");
    node.stop().await;
}

/// A node with staking initialised and the fault proof contract recorded.
async fn penalising_node(dir: &Path) -> Node {
    let node = Node::start_confirmed(dir);
    init_staking(&node).await;
    let penalty = Penalty { initial_bps: 5_000, decay_ms: 864_000_000 };
    let config = FaultProofConfig { contract: AccountId(FAULT_PROOF.0.0), penalty };
    assert_eq!(node.submit(Command::Admin(AdminCommand::SetFaultProof(config))).await, [Event::AdminApplied]);
    node
}

/// The proven fault's event.
fn proven() -> StakingEvent {
    event(3, 2, StakingEventKind::FaultProven { member: MEMBER, turn: 20_400, position: 0, signer_epoch: 1, seq: 42 })
}

fn proven_ref() -> String {
    proven().chain_ref()
}

/// Chains whose registry names the schedule, which records the registry, and whose fault proof
/// contract records `vault`, with a window holding the proven fault.
fn fault_chains(vault: Option<Address>) -> [FakeChain; 3] {
    let chains = quiet_chains(vault);
    for chain in &chains {
        chain.0.lock().unwrap().staking.push(proven());
    }
    chains
}

/// As `fault_chains`, with no event in the window.
fn quiet_chains(vault: Option<Address>) -> [FakeChain; 3] {
    let chains = poll_chains(10);
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.schedule.named = SCHEDULE;
        state.schedule.registry = Some(REGISTRY);
        state.schedule.fault_proof = Some((FAULT_PROOF, vault));
    }
    chains
}

/// Sets what the schedule's governance record names, from the first block, on every chain.
fn record(chains: &[FakeChain; 3], record: GovernanceRecord) {
    for chain in chains {
        chain.0.lock().unwrap().schedule.record = Some((0, record));
    }
}

/// The confirmed watcher over `chains`, configured with the fake chain's registry, the schedule and
/// the fault proof contract.
fn fault_watcher(chains: &[FakeChain; 3], dir: &Path) -> DepositWatcher<Quorum<FakeChain>> {
    let toml = format!(
        r#"{}
        staking = {{ contract = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a", cursor_path = "{}", registry = {{ contract = "{REGISTRY}", schedule = "{SCHEDULE}", fault_proof = {{ contract = "{FAULT_PROOF}", penalty = {{ initial_bps = 5000, decay = "10d" }} }} }} }}
        "#,
        evm_toml(dir, 100),
        dir.join("staking-cursor.json").display()
    );
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap()
}
