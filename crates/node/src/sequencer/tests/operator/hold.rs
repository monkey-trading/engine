//! Which keys can end a benchmark hold when the registry's installation, or a signed rotation, has
//! yet to take effect.

use super::*;

/// A log whose operator is the config's, with its scan at `FRONTIER`, a sequencer recovered over it
/// under `registry`, and trusted time.
fn recovered(dir: &Path, clock: &Arc<TestClock>, registry: Option<RegistryOperator>) -> Sequencer {
    let mut seq = recover_installed(dir, clock.clone(), dev_config().operator().unwrap(), registry).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq
}

/// Whether a hold on a snapshot survives `lift_unsigned_hold` at the clock's time.
fn holds(seq: &mut Sequencer, clock: &TestClock) -> bool {
    seq.benchmark_sync = true;
    seq.lift_unsigned_hold(clock.now_ms());
    seq.benchmark_sync
}

/// The installation of `key` as the log mirrors it.
fn install(key: AccountId) -> Command {
    Command::InstallOperator(RegistryInstallation {
        candidate: AccountId([0xc2; 20]),
        admin_key: key,
        block: 11,
        chain_ref: "0x01:0".into(),
    })
}

#[test]
fn re_seating_the_recorded_key_supersedes_a_due_rotation_to_another_key() {
    // The recorded key signed a rotation to another, whose notice has run out; the other's key is
    // lost, and the timelock seats the recorded key again before the log's scan reaches it.
    let (dir, clock) = staked_log("reseat-over-rotation", Some(FRONTIER));
    let recorded = dev_config().operator().unwrap();
    let other = address(INSTALLED_SEED);
    let mut seq = recovered(&dir, &clock, None);
    let rotation = signed_by(&seq, &clock, RECORDED_SEED, AdminCommand::RotateOperator { address: other });
    let events = seq.sequence(rotation).unwrap().expect("admitted");
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    seq.flush().unwrap();
    drop(seq);
    clock.advance(Duration::from_millis(NOTICE_MS as u64 + 1_000));

    let reseated = RegistryOperator { admin_key: recorded.address, since: FRONTIER + 1_000 };
    let mut seq = recovered(&dir, &clock, Some(reseated));
    assert_eq!(reseated.unscanned(&seq.engine), Some(recorded.address));
    assert_eq!(seq.engine.due_rotation(clock.now_ms()).map(|rotation| rotation.address), Some(other));
    assert!(!holds(&mut seq, &clock), "a hold waits for a snapshot only the superseded key could sign");
    // Without the registry's seating the due key signs, so the hold stands.
    let mut seq = recovered(&dir, &clock, None);
    assert!(holds(&mut seq, &clock));
}

#[test]
fn an_unscanned_installation_whose_own_key_is_due_keeps_the_hold() {
    let (dir, clock) = staked_log("seating-due", Some(FRONTIER));
    let key = address(INSTALLED_SEED);
    let mut seq = recovered(&dir, &clock, None);
    seq.sequence(install(key)).unwrap().expect("admitted");
    seq.flush().unwrap();
    drop(seq);
    clock.advance(Duration::from_millis(NOTICE_MS as u64 + 1_000));

    // The scan has not passed the seating, and the installed key's notice has run out: it can sign.
    let seating = RegistryOperator { admin_key: key, since: FRONTIER + 1_000 };
    let mut seq = recovered(&dir, &clock, Some(seating));
    assert_eq!(seating.unscanned(&seq.engine), Some(key));
    assert_eq!(seq.engine.due_rotation(clock.now_ms()).map(|rotation| rotation.address), Some(key));
    assert!(holds(&mut seq, &clock), "the due installed key can end the hold");
}

#[test]
fn a_rotation_back_to_a_formerly_installed_key_keeps_the_hold() {
    // The installation of `key` handed over and `key` handed on to the recorded key; the recorded
    // key now signs a rotation back to it. The log still records the installation, but its handover
    // is long done, so the rotation is a signed one and the recorded key still signs meanwhile.
    let (dir, clock) = staked_log("rotation-back", Some(FRONTIER));
    let recorded = dev_config().operator().unwrap();
    let key = address(INSTALLED_SEED);
    let mut seq = recovered(&dir, &clock, None);
    seq.sequence(install(key)).unwrap().expect("admitted");
    // The installed key's notice runs out and it takes over; it rotates to the recorded key.
    clock.advance(Duration::from_millis(NOTICE_MS as u64 + 1_000));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let to_recorded =
        signed_by(&seq, &clock, INSTALLED_SEED, AdminCommand::RotateOperator { address: recorded.address });
    let events = seq.sequence(to_recorded).unwrap().expect("admitted");
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    assert_eq!(seq.engine.operator().map(|operator| operator.address), Some(key));
    clock.advance(Duration::from_millis(NOTICE_MS as u64 + 1_000));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let back = signed_by(&seq, &clock, RECORDED_SEED, AdminCommand::RotateOperator { address: key });
    let events = seq.sequence(back).unwrap().expect("admitted");
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    assert_eq!(seq.engine.operator().map(|operator| operator.address), Some(recorded.address));
    let installed = seq.engine.operator_election().installed.expect("the installation is recorded");
    let pending = seq.engine.operator_bounds().rotation.expect("the rotation back is pending");
    assert_eq!((installed.admin_key, pending.address), (key, key));
    assert_ne!(installed.effective_at, pending.effective_at);
    assert!(holds(&mut seq, &clock), "the rotation back is not the installation's handover");
}

#[test]
fn a_hold_that_spans_a_turns_boundary_takes_the_snapshot_the_turns_key_signs() {
    // The log assigned a turn to the member key, and a benchmark hold waits across its boundary: the
    // snapshot the member's key signs ends it, and the previous turn's key signs nothing.
    let (dir, clock, _, member) = turn_log("turn-hold");
    let recorded = dev_config().operator().unwrap();
    let mut seq = recover_turns(&dir, clock.clone(), Operator { address: member, ..recorded }, B256::repeat_byte(0x54));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.wait_for_benchmarks().unwrap();
    assert_eq!(seq.engine.due_key(clock.now_ms()).map(|due| due.address), Some(member));
    assert!(holds(&mut seq, &clock), "a due turn's key can sign the snapshot, so the hold stands");
    let refused = seq.sequence(snapshot(&seq, &clock, RECORDED_SEED)).unwrap().expect("admitted");
    assert!(matches!(refused.last(), Some(Event::AdminRejected { .. })), "{refused:?}");
    assert!(seq.benchmark_sync, "the previous turn's key ended the hold");
    let applied = seq.sequence(snapshot(&seq, &clock, INSTALLED_SEED)).unwrap().expect("admitted");
    assert!(applied.contains(&Event::AdminApplied), "{applied:?}");
    assert!(!seq.benchmark_sync, "the turn's key ended the hold");
}
