//! Operator faults from `V70`: the config's fault proof contract recorded once, a log recording
//! another refused, and a passed activation of `V70` followed by the record.

use super::*;

use engine_core::governance::{GovernanceConfig, PassedProposal, command_hash};
use engine_core::operator_faults::{FaultProofConfig, Penalty};

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
    // A log under the version before, with rotation on, so only a vote activates `V70`.
    let dir = scratch_dir("faults-voted");
    let clock = Arc::new(TestClock::at(START));
    let mut seq = recover_penalising(&dir, clock.clone(), Some(config(5_000))).unwrap();
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
    seq.trust(sample_around(clock.as_ref())).unwrap();
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

/// Recovers the log in `dir` under the dev config's operator, with the vote contract and the fault
/// proof contract `fault_proof`.
/// Error cases:
/// - recovery fails (see `Sequencer::recover`)
fn recover_penalising(
    dir: &Path,
    clock: Arc<TestClock>,
    fault_proof: Option<FaultProofConfig>,
) -> anyhow::Result<Sequencer> {
    let genesis = finished_genesis(&log_entries(dir));
    let (manifest, _) = watch::channel(Manifest::default());
    let operator = dev_config().operator().unwrap();
    let base = sequencer_config(dir, manifest);
    let governance = Some(governance());
    let cfg = SequencerConfig { clock, genesis, operator, governance, fault_proof, ..base };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, mpsc::unbounded_channel().0)
}
