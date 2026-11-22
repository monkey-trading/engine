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
fn re_seating_the_recorded_key_supersedes_a_due_election_behind_its_re_election() {
    // From `V88`: a catch-up mirrors the recorded key's re-election and, before it takes effect, the next
    // election, of another key, which waits behind it with no rotation pending. Both come due; the other's
    // key is lost, and the timelock turns rotation off and seats the recorded key again before the log's
    // scan reaches it.
    let (dir, clock) = staked_log("reseat-over-election", Some(FRONTIER));
    let recorded = dev_config().operator().unwrap();
    queue_elections(&dir, &clock, [RECORDED_SEED, INSTALLED_SEED]);
    clock.advance(Duration::from_millis(NOTICE_MS as u64 + 1_000));

    let reseated = RegistryOperator { admin_key: recorded.address, since: FRONTIER + 1_000 };
    let mut seq = recovered(&dir, &clock, Some(reseated));
    assert_eq!(reseated.unscanned(&seq.engine), Some(recorded.address));
    let due = seq.engine.due_rotation(clock.now_ms()).map(|rotation| rotation.address);
    assert_eq!(due, Some(address(INSTALLED_SEED)));
    assert!(!holds(&mut seq, &clock), "a hold waits for a snapshot only the superseded key could sign");
    // Without the registry's seating the due key signs, so the hold stands.
    let mut seq = recovered(&dir, &clock, None);
    assert!(holds(&mut seq, &clock));
}

#[test]
fn re_seating_the_recorded_key_supersedes_no_election_of_it() {
    // From `V88`: the recorded key's two re-elections wait in turn and hand over to no other key.
    let (dir, clock) = staked_log("reseat-over-re-elections", Some(FRONTIER));
    queue_elections(&dir, &clock, [RECORDED_SEED, RECORDED_SEED]);
    let reseated = RegistryOperator { admin_key: dev_config().operator().unwrap().address, since: FRONTIER + 1_000 };
    let seq = recovered(&dir, &clock, Some(reseated));
    assert_eq!(seq.engine.operator_election().waiting.len(), 1);
    assert_eq!(reseated.unscanned(&seq.engine), None);
}

/// Mirrors, with rotation on, the elections of `seeds`' keys before the first takes effect, both seated on
/// chain before the scan's frontier, then rotation turning off with no cancellation: the first waits ahead of
/// the second.
fn queue_elections(dir: &Path, clock: &Arc<TestClock>, seeds: [u8; 2]) {
    let mut seq = recovered(dir, clock, None);
    let on =
        Command::SwitchRotation { enabled: true, election_cancelled: false, block: 11, chain_ref: "0x0a:0".into() };
    let elections = (1..).zip(seeds).map(|(epoch, seed): (u64, u8)| {
        Command::ElectOperator(RegistryElection {
            epoch,
            candidate: AccountId([seed; 20]),
            admin_key: address(seed),
            effective_at: FRONTIER - i64::try_from(3 - epoch).unwrap() * 3_600_000,
            block: 11 + epoch,
            chain_ref: format!("0x0b:{epoch}"),
        })
    });
    let off =
        Command::SwitchRotation { enabled: false, election_cancelled: false, block: 14, chain_ref: "0x0c:0".into() };
    for cmd in std::iter::once(on).chain(elections).chain([off]) {
        let events = seq.sequence(cmd).unwrap().expect("admitted");
        assert!(!events.iter().any(|event| matches!(event, Event::ElectionRejected { .. })), "{events:?}");
    }
    assert_eq!(seq.engine.operator_election().waiting.len(), 1);
    assert_eq!(seq.engine.operator_bounds().rotation, None, "the waiting re-election names the recorded key");
    seq.flush().unwrap();
}

/// A promoted member's admin key, seated from the promotion's block that the log's scan has yet to
/// reach, ends a hold only the outgoing operator's key could end: holding would keep out the mirror
/// that hands the log over to the promoted key.
#[test]
fn a_promoted_key_the_scan_has_not_reached_lifts_a_hold_the_outgoing_key_alone_could_end() {
    let (dir, clock) = staked_log("promoted-seating", Some(FRONTIER));
    let promoted = RegistryOperator { admin_key: address(INSTALLED_SEED), since: FRONTIER + 1_000 };
    let mut seq = recovered(&dir, &clock, Some(promoted));
    assert_eq!(promoted.unscanned(&seq.engine), Some(promoted.admin_key));
    assert!(!holds(&mut seq, &clock), "a hold waits for a snapshot only the outgoing key could sign");
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

#[test]
fn no_benchmark_hold_waits_on_a_key_an_installation_supersedes_until_the_installed_key_is_due() {
    let (dir, clock) = staked_log("installed-hold", Some(FRONTIER));
    let recorded = dev_config().operator().unwrap();
    let key = address(INSTALLED_SEED);
    let seating = RegistryOperator { admin_key: key, since: FRONTIER + 1_000 };
    let seated = Some(seating);
    let reseated = Some(RegistryOperator { admin_key: recorded.address, since: FRONTIER + 1_000 });
    // With no installation, or the registry seating the recorded key again, a hold drops what
    // follows it: at startup, after trusted time recovers over a rollover, and at a changed
    // schedule's rollover. With an installation the log has yet to mirror, each hold ends at the
    // next stamp, and sequencing goes on.
    for (registry, held) in [(None, true), (reseated, true), (seated, false)] {
        let (logs, _guard) = Logs::capture();
        let mut seq = start_over_a_rollover(&dir, &clock, recorded, registry);
        assert_eq!(seq.sequence(Command::Tick).unwrap().is_none(), held, "{registry:?}");
        assert_eq!(seq.benchmark_sync, held, "{registry:?}");
        let due = seq.engine.next_rollover().expect("a rollover is scheduled");
        seq.benchmark_sync = false;
        seq.benchmark_time_refused = true;
        seq.note_benchmark_time_admission(true, due);
        assert!(seq.benchmark_sync, "trusted time recovered over a rollover: {registry:?}");
        assert_eq!(seq.sequence(Command::Tick).unwrap().is_none(), held, "{registry:?}");
        seq.benchmark_sync = false;
        seq.benchmark_schedule_pending = true;
        seq.admit_benchmark_schedule(due);
        assert!(seq.benchmark_sync, "a changed schedule's rollover came due: {registry:?}");
        assert_eq!(seq.sequence(Command::Tick).unwrap().is_none(), held, "{registry:?}");
        assert_eq!((seq.benchmark_sync, seq.benchmark_schedule_pending), (held, held), "{registry:?}");
        let ended = if held { 0 } else { 3 };
        assert_eq!(logs.warnings("benchmark_sync_pending"), 3, "{registry:?}");
        assert_eq!(logs.count("alert_resolved=\"benchmark_sync_pending\""), ended, "{registry:?}");
        assert_eq!(logs.warnings("benchmark_sync_skipped"), ended, "{registry:?}");
        if !held {
            // The recorded key may still sign, and its snapshot ends the skipped episode.
            let events = seq.sequence(snapshot(&seq, &clock, RECORDED_SEED)).unwrap().expect("admitted");
            assert!(events.contains(&Event::AdminApplied), "{events:?}");
            assert_eq!(logs.count("alert_resolved=\"benchmark_sync_skipped\""), 1);
        }
        seq.flush().unwrap();
    }

    // The installation's mirror sequences. While its handover waits out the notice no hold waits on
    // the replaced key, with the registry read or without, and once the scan has passed it too.
    let mut seq = start_over_a_rollover(&dir, &clock, recorded, seated);
    assert_eq!(seating.unscanned(&seq.engine), Some(key));
    let install = RegistryInstallation {
        candidate: AccountId([0xc2; 20]),
        admin_key: key,
        block: 11,
        chain_ref: "0x01:0".into(),
    };
    let events = seq.sequence(Command::InstallOperator(install)).unwrap().expect("admitted past the hold");
    assert!(events.iter().any(|event| matches!(event, Event::OperatorInstalled { .. })), "{events:?}");
    assert_eq!(seating.unscanned(&seq.engine), Some(key), "the scan has not passed the seating");
    let progress = Command::StakingProgress {
        block: 12,
        block_hash: B256::repeat_byte(2),
        block_time: Some(FRONTIER + 2_000),
        named_turn: None,
    };
    let events = seq.sequence(progress).unwrap().expect("admitted");
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    assert_eq!(seating.unscanned(&seq.engine), None);
    seq.flush().unwrap();
    drop(seq);
    for registry in [None, seated] {
        let mut seq = start_over_a_rollover(&dir, &clock, recorded, registry);
        assert!(seq.sequence(Command::Tick).unwrap().is_some(), "{registry:?}");
        assert!(!seq.benchmark_sync, "{registry:?}");
        seq.flush().unwrap();
    }

    // Once the installed key is due, a hold waits for its snapshot, which ends the hold, hands the
    // key over, and ends the skipped episode an earlier start may have left open.
    clock.advance(Duration::from_millis(NOTICE_MS as u64 + 1_000));
    let (logs, _guard) = Logs::capture();
    let mut seq = start_over_a_rollover(&dir, &clock, recorded, seated);
    assert!(seq.sequence(Command::Tick).unwrap().is_none(), "the installed key can sign");
    let events = seq.sequence(snapshot(&seq, &clock, INSTALLED_SEED)).unwrap().expect("admitted");
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    assert!(!seq.benchmark_sync);
    assert_eq!(seq.engine.operator().map(|operator| operator.address), Some(key));
    assert_eq!(logs.count("alert_resolved=\"benchmark_sync_skipped\""), 1);
    // A later hold waits on the installed key, which signs now.
    seq.benchmark_time_refused = true;
    seq.note_benchmark_time_admission(true, seq.engine.next_rollover().expect("a rollover is scheduled"));
    assert!(seq.sequence(Command::Tick).unwrap().is_none(), "the handover is done, and supersedes nothing");

    // A log whose scan records no frontier time holds as before.
    let (dir, clock) = staked_log("installed-hold-unscanned", None);
    let mut seq = start_over_a_rollover(&dir, &clock, recorded, seated);
    assert!(seq.sequence(Command::Tick).unwrap().is_none());
}
