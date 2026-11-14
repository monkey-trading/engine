//! A log whose key a daily turn names, for the tests of a turn's `TurnTaken`, its handover and its log
//! check.

use super::*;

/// A staked log with rotation on, a set naming the member key mirrored, and a turn for it mirrored
/// an hour before its boundary, which the returned clock is a second past: the log's directory, the
/// clock, the turn and the member's key.
pub(crate) fn turn_log(name: &str) -> (PathBuf, Arc<TestClock>, u64, AccountId) {
    let (dir, clock, turn, member) = turn_log_under(name, &[], None);
    clock.advance(Duration::from_millis((boundary(turn).unwrap() + 1_000 - clock.now_ms()) as u64));
    (dir, clock, turn, member)
}

/// `turn_log`, with `others` in the set beside the member and the access policy `policy` recorded
/// before rotation, an hour before the turn's boundary.
pub(crate) fn turn_log_under(
    name: &str,
    others: &[SetMember],
    policy: Option<AccessPolicy>,
) -> (PathBuf, Arc<TestClock>, u64, AccountId) {
    let (dir, clock) = staked_log_under(name, Some(FRONTIER), policy.clone());
    let member = address(INSTALLED_SEED);
    let candidate = AccountId([0xc1; 20]);
    let recorded = dev_config().operator().unwrap();
    let mut seq = recover_installed_under(&dir, clock.clone(), recorded, None, policy).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let mut members = vec![SetMember { candidate, admin_key: member, weight: TokenUnits(1) }];
    members.extend_from_slice(others);
    let effective_at = clock.now_ms() + NOTICE_MS;
    let (block, chain_ref) = (12, "0x02:0".to_owned());
    let adoption = RegistryRotationSet { set: 1, epoch: 1, effective_at, quorum: true, members, block, chain_ref };
    let switch =
        Command::SwitchRotation { enabled: true, election_cancelled: false, block: 11, chain_ref: "0x01:0".into() };
    for cmd in [switch, Command::AdoptRotationSet(adoption)] {
        let events = seq.sequence(cmd).unwrap().expect("admitted");
        assert!(!events.iter().any(|event| matches!(event, Event::ElectionRejected { .. })), "{events:?}");
    }
    let turn = (0..).find(|turn| boundary(*turn).unwrap() > clock.now_ms() + NOTICE_MS + 86_400_000).unwrap();
    let begins = boundary(turn).unwrap();
    clock.advance(Duration::from_millis((begins - 3_600_000 - clock.now_ms()) as u64));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let (reason, block, chain_ref) = (TurnReason::Draw, 13, "0x03:0".to_owned());
    let assignment = RegistryTurn { turn, set: 1, position: 0, reason, candidate, admin_key: member, block, chain_ref };
    let events = seq.sequence(Command::AssignTurn(assignment)).unwrap().expect("admitted");
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    seq.flush().unwrap();
    (dir, clock, turn, member)
}

/// Recovers the log in `dir` under a config naming `operator`, whose enclave runs `release`.
pub(crate) fn recover_turns(dir: &Path, clock: Arc<TestClock>, operator: Operator, release: B256) -> Sequencer {
    let genesis = finished_genesis(&log_entries(dir));
    let (manifest, _) = watch::channel(Manifest::default());
    let base = sequencer_config(dir, manifest);
    let cfg = SequencerConfig { clock, genesis, operator, turn_release: Some(release), ..base };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, mpsc::unbounded_channel().0)
        .unwrap()
}
