//! Operator faults from `V70`: the config's fault proof contract recorded once, a log recording
//! another refused, a passed activation of `V70` followed by the record, and neither the vote contract
//! nor the fault proof contract recorded while the schedule's governance record names another.

use super::*;

use alloy::primitives::Address;
use engine_core::governance::{GovernanceConfig, PassedProposal, command_hash};
use engine_core::operator_faults::{FaultProofConfig, Penalty};

use crate::custody::GovernanceRecord;

const VOTE: AccountId = AccountId([0x5f; 20]);

fn config(initial_bps: i64) -> FaultProofConfig {
    FaultProofConfig { contract: AccountId([0x5e; 20]), penalty: Penalty { initial_bps, decay_ms: 864_000_000 } }
}

fn governance() -> GovernanceConfig {
    GovernanceConfig { vote: VOTE, restriction_ttl_ms: 3_600_000 }
}

#[test]
fn the_config_fault_proof_contract_is_recorded_once_and_another_is_refused() {
    let (dir, clock) = staked_log("faults-recorded", Some(FRONTIER));
    let activate = |fault_proof| {
        let mut seq = recover_penalising(&dir, clock.clone(), fault_proof)?;
        seq.activate_protocol()?;
        seq.trust(sample_around(clock.as_ref()))
    };
    activate(Some(config(5_000))).unwrap();
    let recorded = commands(&log_entries(&dir));
    assert_eq!(recorded.last(), Some(&Command::Admin(AdminCommand::SetFaultProof(config(5_000)))));
    // `GET /v1/house/state` names the recorded contract and penalty.
    let mut seq = recover_penalising(&dir, clock.clone(), Some(config(5_000))).unwrap();
    let (reply, mut answer) = oneshot::channel();
    seq.house.wait(reply);
    seq.flush().unwrap();
    let state = serde_json::to_value(answer.try_recv().expect("a flush answers the read")).unwrap();
    let faults = &state["operator_faults"]["config"];
    assert_eq!(faults["penalty"], serde_json::json!({ "initial_bps": 5_000, "decay_ms": 864_000_000 }), "{state}");
    drop(seq);
    activate(Some(config(5_000))).unwrap();
    assert_eq!(commands(&log_entries(&dir)).len(), recorded.len(), "recorded once");
    for other in [Some(config(2_500)), None] {
        let error = activate(other).unwrap_err();
        assert!(format!("{error:#}").contains("the log records fault proof contract"), "{error:#}");
    }
    assert_eq!(commands(&log_entries(&dir)).len(), recorded.len(), "nothing written");
}

#[test]
fn a_passed_activation_of_v70_is_followed_by_the_fault_proof_contract() {
    let dir = scratch_dir("faults-voted");
    let clock = Arc::new(TestClock::at(START));
    let mut seq = recover_penalising(&dir, clock.clone(), Some(config(5_000))).unwrap();
    let applies_at = voted_v70(&mut seq, &clock);
    clock.advance(Duration::from_millis((applies_at - clock.now_ms()) as u64));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    seq.flush().unwrap();
    let written = commands(&log_entries(&dir));
    let applied = written.iter().position(|cmd| matches!(cmd, Command::ApplyProposal(voted) if voted.proposal == 3));
    let applied = applied.expect("the activation is sequenced");
    let recorded = Command::Admin(AdminCommand::SetFaultProof(config(5_000)));
    assert_eq!(written[applied + 1..], [recorded, Command::Tick], "recorded after it, once");
    assert_eq!(seq.engine.protocol_version(), ProtocolVersion::V70);
    assert_eq!(seq.engine.operator_faults().config, Some(config(5_000)));
}

#[test]
fn a_contract_the_schedules_record_does_not_name_is_refused_before_anything_reaches_the_log() {
    let (dir, clock) = staked_log("faults-record-refused", Some(FRONTIER));
    let written = commands(&log_entries(&dir));
    let (vote, fault_proof) = (Address::from(VOTE.0), Address::from(config(5_000).contract.0));
    let timelocks = Address::repeat_byte(0x6e);
    // The record names the timelock's contracts, and the config a lookalike of the fault proof contract, then
    // of the vote contract.
    let lookalikes = [
        (GovernanceRecord::Set { vote, fault_proof: timelocks }, "fault proof"),
        (GovernanceRecord::Set { vote: timelocks, fault_proof }, "vote"),
    ];
    for (record, role) in lookalikes {
        let (_published, held) = watch::channel(record);
        let mut seq = recover_recorded(&dir, clock.clone(), Some(config(5_000)), Some(held)).unwrap();
        let error = seq.activate_protocol().unwrap_err();
        assert!(format!("{error:#}").contains(&format!("names {role} contract {timelocks}")), "{error:#}");
        seq.trust(sample_around(clock.as_ref())).unwrap();
        drop(seq);
        assert_eq!(commands(&log_entries(&dir)), written, "a command reached the log");
    }
}

#[test]
fn a_record_naming_the_configured_contracts_or_none_or_kept_by_no_schedule_lets_them_be_recorded() {
    let (vote, fault_proof) = (Address::from(VOTE.0), Address::from(config(5_000).contract.0));
    let records = [GovernanceRecord::Set { vote, fault_proof }, GovernanceRecord::Unset, GovernanceRecord::Absent];
    for (index, record) in records.into_iter().enumerate() {
        let (dir, clock) = staked_log(&format!("faults-record-allowed-{index}"), Some(FRONTIER));
        let (_published, held) = watch::channel(record);
        let mut seq = recover_recorded(&dir, clock.clone(), Some(config(5_000)), Some(held)).unwrap();
        seq.activate_protocol().unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        drop(seq);
        let recorded = commands(&log_entries(&dir));
        let tail = [
            Command::Admin(AdminCommand::SetGovernance(governance())),
            Command::Admin(AdminCommand::SetFaultProof(config(5_000))),
        ];
        assert!(recorded.ends_with(&tail), "{record:?}: {:?}", &recorded[recorded.len().saturating_sub(2)..]);
    }
}

#[test]
fn a_passed_activation_of_v70_records_no_fault_proof_contract_the_latest_record_does_not_name() {
    let dir = scratch_dir("faults-voted-record");
    let clock = Arc::new(TestClock::at(START));
    // The record names nothing at the start, so the vote contract is recorded.
    let (published, held) = watch::channel(GovernanceRecord::Unset);
    let mut seq = recover_recorded(&dir, clock.clone(), Some(config(5_000)), Some(held)).unwrap();
    let applies_at = voted_v70(&mut seq, &clock);
    // The custody watcher reads the timelock's record of another fault proof contract before the activation applies.
    let vote = Address::from(VOTE.0);
    published.send_replace(GovernanceRecord::Set { vote, fault_proof: Address::repeat_byte(0x6e) });
    clock.advance(Duration::from_millis((applies_at - clock.now_ms()) as u64));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let error = seq.sequence(Command::Tick).unwrap_err();
    assert!(format!("{error:#}").contains("names fault proof contract"), "{error:#}");
    drop(seq);
    let written = commands(&log_entries(&dir));
    assert!(
        written.iter().any(|cmd| matches!(cmd, Command::ApplyProposal(voted) if voted.proposal == 3)),
        "not activated"
    );
    let recorded = |cmd: &Command| matches!(cmd, Command::Admin(AdminCommand::SetFaultProof(_)));
    assert!(!written.iter().any(recorded), "the fault proof contract reached the log");
}

/// Recovers the log in `dir` under the dev config's operator, with the vote contract and the fault
/// proof contract `fault_proof`.
/// Error cases:
/// - recovery fails (see `Sequencer::recover`)
fn recover_penalising(
    dir: &Path,
    clock: Arc<TestClock>,
    fault_proof: Option<FaultProofConfig>,
) -> anyhow::Result<Sequencer> {
    recover_recorded(dir, clock, fault_proof, None)
}

/// `recover_penalising`, checking each record of the configured contracts against the governance record
/// `governance_record` holds.
/// Error cases:
/// - recovery fails (see `Sequencer::recover`)
fn recover_recorded(
    dir: &Path,
    clock: Arc<TestClock>,
    fault_proof: Option<FaultProofConfig>,
    governance_record: Option<watch::Receiver<GovernanceRecord>>,
) -> anyhow::Result<Sequencer> {
    let genesis = finished_genesis(&log_entries(dir));
    let (manifest, _) = watch::channel(Manifest::default());
    let operator = dev_config().operator().unwrap();
    let base = sequencer_config(dir, manifest);
    let governance = Some(governance());
    let cfg = SequencerConfig { clock, genesis, operator, governance, fault_proof, governance_record, ..base };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, mpsc::unbounded_channel().0)
}

/// Takes `seq`'s fresh log, under the version before `V70` with rotation on, so that only a vote activates `V70`,
/// to the vote's passed activation, with the vote contract recorded, and returns when the activation applies.
fn voted_v70(seq: &mut Sequencer, clock: &TestClock) -> Millis {
    seq.activate_protocol_at(ProtocolVersion::V63).unwrap();
    let mut genesis = dev_config().genesis;
    genesis.benchmarks.clear();
    genesis.staking = Some(StakingConfig {
        liquidity_floor: Fixed::from_int(1),
        epoch_ms: 86_400_000,
        start_ms: START,
        cooldown_ms: 1,
    });
    seq.genesis(&genesis).unwrap();
    seq.trust(sample_around(clock)).unwrap();
    let block_hash = B256::repeat_byte(1);
    let progress = Command::StakingProgress { block: 10, block_hash, block_time: Some(FRONTIER), named_turn: None };
    seq.sequence(progress).unwrap().expect("admitted");
    let switch =
        Command::SwitchRotation { enabled: true, election_cancelled: false, block: 11, chain_ref: "0x01:0".into() };
    seq.sequence(switch).unwrap().expect("admitted");
    let recorded = seq.sequence(Command::Admin(AdminCommand::SetGovernance(governance()))).unwrap();
    assert_eq!(recorded.expect("admitted").last(), Some(&Event::AdminApplied));
    assert_eq!(seq.engine.operator_faults().config, None, "nothing recorded before V70");
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V70 };
    let passed = PassedProposal {
        proposal: 3,
        fast: false,
        command_hash: command_hash(&activate),
        command: Some(activate),
        block: 12,
        chain_ref: "0x03:0".into(),
    };
    let events = seq.sequence(Command::PassProposal(Box::new(passed))).unwrap().expect("admitted");
    let Some(Event::ProposalScheduled { applies_at, .. }) = events.last().cloned() else { panic!("{events:?}") };
    applies_at
}
