//! Recovery, activation, and replay with the operator the log records.

use super::*;

#[test]
fn a_v7_log_records_the_operator_and_activates_the_latest_version_at_its_next_seq() {
    let dir = scratch_dir("activation-v7");
    let client_auth = ClientAuth::Eip712 { chain_id: 31337, vault: AccountId([0; 20]) };
    let v7 = vec![
        Command::Admin(AdminCommand::SetClientAuth(client_auth.clone())),
        Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V3 }),
        Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V7 }),
    ];
    recover(&dir, &client_auth).sequence_required("V7 activation", v7).unwrap();
    recover(&dir, &client_auth).activate_protocol().unwrap();
    let written = log_entries(&dir);
    let cmds = commands(&written);
    assert_eq!(
        cmds[3..],
        [
            Command::Admin(AdminCommand::SetOperator(dev_config().operator().unwrap())),
            Command::Admin(AdminCommand::ActivateCustodyFinality {
                version: ProtocolVersion::LATEST,
                legacy_deposits: vec![],
            }),
        ]
    );
    let replayed = replay(&written).unwrap();
    assert_eq!(
        replayed.activations,
        [(ProtocolVersion::V3, 1), (ProtocolVersion::V7, 2), (ProtocolVersion::LATEST, 4)]
    );
}

#[test]
fn a_log_recording_another_operator_refuses_to_start_and_writes_nothing() {
    let dir = scratch_dir("activation-other-operator");
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    let written = log_entries(&dir).len();
    let operator = dev_config().operator().unwrap();
    for other in [Operator { address: AccountId([9; 20]), ..operator }, Operator { chain_id: 1, ..operator }] {
        let (stream, _) = broadcast::channel(16);
        let (manifest, _) = watch::channel(Manifest::default());
        let cfg = SequencerConfig { client_auth: signed(31_337), operator: other, ..sequencer_config(&dir, manifest) };
        let recovered = Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, mpsc::unbounded_channel().0);
        let err = recovered.err().unwrap();
        assert!(format!("{err:#}").contains("the log records operator"), "{err:#}");
    }
    assert_eq!(log_entries(&dir).len(), written);
}

#[test]
fn a_log_claiming_an_unsigned_operator_command_applied_after_funds_does_not_replay() {
    let dir = scratch_dir("unsigned-admin");
    let mut seq = recover(&dir, &ClientAuth::Unsigned);
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.sequence(deposit(100, "dep-1")).unwrap();
    let every_market = Command::Admin(AdminCommand::SetReduceOnly { market: None, reduce_only: true });
    let events = seq.sequence(every_market.clone()).unwrap().expect("trusted time admits a stamp");
    assert!(matches!(&events[..], [Event::AdminRejected { .. }]), "{events:?}");
    seq.flush().unwrap();
    drop(seq);

    // As if the host rewrote the entry to say it applied.
    let mut entries = log_entries(&dir);
    let forged = entries.iter_mut().find_map(|entry| match entry {
        LogEntry::Command(c) if c.sequenced.cmd == every_market => Some(c),
        _ => None,
    });
    let forged = forged.expect("the command is logged");
    forged.events = Some(vec![Event::AdminApplied]);
    let err = replay(&entries).err().unwrap();
    assert!(format!("{err:#}").contains("invalid command origin signature"), "{err:#}");
    // Without its seal, as in a historical entry, replay still refuses the rewritten events.
    let unsealed = entries.iter_mut().find_map(|entry| match entry {
        LogEntry::Command(c) if c.sequenced.cmd == every_market => Some(c),
        _ => None,
    });
    unsealed.expect("the command is logged").seal = None;
    let err = replay(&entries).err().unwrap();
    assert!(format!("{err:#}").contains("differ from the events replay gives"), "{err:#}");
}
