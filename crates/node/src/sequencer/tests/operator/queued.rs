//! From `V88`, a start under the key of an election waiting to take effect: the pending handover is the
//! first waiting election's, and each election behind it names a successor too, as the registry seats
//! them in turn.

use super::*;

/// The admin keys three elections name in turn: another candidate, the recorded operator again, and a
/// third, as an election, a re-election and a third election before the first takes effect.
const ELECTED: [u8; 3] = [0x61, RECORDED_SEED, 0x63];

#[test]
fn a_start_under_a_key_an_election_waiting_names_recovers() {
    let (dir, clock) = queued_log("queued-elections");
    let recorded = dev_config().operator().unwrap();
    for seed in [ELECTED[0], ELECTED[2]] {
        let started = recover_installed(&dir, clock.clone(), Operator { address: address(seed), ..recorded }, None);
        assert!(started.is_ok(), "{seed:#x}: {:#}", started.err().unwrap());
    }
    let stranger = recover_installed(&dir, clock.clone(), Operator { address: address(0x64), ..recorded }, None);
    let err = stranger.err().expect("a key no election names");
    assert!(format!("{err:#}").contains("the log records operator"), "{err:#}");
    let other_vault = Operator { address: address(ELECTED[2]), vault: AccountId([9; 20]), ..recorded };
    let err = recover_installed(&dir, clock.clone(), other_vault, None).err().expect("another deployment");
    assert!(format!("{err:#}").contains("the log records operator"), "{err:#}");
}

#[test]
fn a_cancelled_election_names_no_successor() {
    let (dir, clock) = queued_log("queued-elections-cancelled");
    let recorded = dev_config().operator().unwrap();
    let mut seq = recover_installed(&dir, clock.clone(), recorded, None).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let off =
        Command::SwitchRotation { enabled: false, election_cancelled: true, block: 20, chain_ref: "0x0c:0".into() };
    let events = seq.sequence(off).unwrap().expect("admitted");
    assert!(!events.iter().any(|event| matches!(event, Event::ElectionRejected { .. })), "{events:?}");
    seq.flush().unwrap();
    drop(seq);
    let third = Operator { address: address(ELECTED[2]), ..recorded };
    let err = recover_installed(&dir, clock.clone(), third, None).err().expect("the cancelled election's key");
    assert!(format!("{err:#}").contains("the log records operator"), "{err:#}");
    assert!(recover_installed(&dir, clock, Operator { address: address(ELECTED[0]), ..recorded }, None).is_ok());
}

/// A staked log with rotation on and `ELECTED`'s three elections mirrored before the first's time: the
/// first two wait behind the third, and the pending handover is the first's.
fn queued_log(name: &str) -> (PathBuf, Arc<TestClock>) {
    let (dir, clock) = staked_log(name, Some(FRONTIER));
    let mut seq = recover_installed(&dir, clock.clone(), dev_config().operator().unwrap(), None).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let on =
        Command::SwitchRotation { enabled: true, election_cancelled: false, block: 11, chain_ref: "0x0a:0".into() };
    let elections = (1..).zip(ELECTED).map(|(epoch, seed): (u64, u8)| {
        Command::ElectOperator(RegistryElection {
            epoch,
            candidate: AccountId([seed; 20]),
            admin_key: address(seed),
            effective_at: clock.now_ms() + NOTICE_MS + i64::try_from(epoch).unwrap() * 3_600_000,
            block: 11 + epoch,
            chain_ref: format!("0x0b:{epoch}"),
        })
    });
    for cmd in std::iter::once(on).chain(elections) {
        let events = seq.sequence(cmd).unwrap().expect("admitted");
        assert!(!events.iter().any(|event| matches!(event, Event::ElectionRejected { .. })), "{events:?}");
    }
    assert_eq!(seq.engine.operator_election().waiting.len(), 2, "the first two wait behind the third");
    assert_eq!(seq.engine.operator_bounds().rotation.map(|rotation| rotation.address), Some(address(ELECTED[0])));
    seq.flush().unwrap();
    (dir, clock)
}
