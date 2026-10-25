use super::*;

use std::collections::BTreeSet;
use std::hint::black_box;
use std::time::{Duration, Instant};

use crate::checkpoints::accounting::tests::{approved, assert_matches, recompute_work};
use crate::sequencer::fixture_tests::signed_fixture;
use crate::tee::EnclaveKey;

#[test]
fn accounting_agrees_with_full_recomputation_for_every_immutable_historical_fixture() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths: Vec<_> = fs::read_dir(root.join("tests/reference"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    paths.push(root.join("tests/fixtures/v1-dev.jsonl"));
    paths.sort();
    assert!(paths.len() >= 18, "every shipped protocol fixture plus the pre-event development log");
    let mut versions = BTreeSet::new();
    let mut pre_event = false;
    for path in paths {
        let entries = signed_fixture(&path);
        pre_event |= entries.iter().any(|entry| matches!(entry, LogEntry::Command(c) if c.events.is_none()));
        let replayed = assert_replay(&entries);
        versions.insert(replayed.engine.protocol_version());
        assert!(replayed.commands > 0 && replayed.checkpoints > 0, "{}", path.display());
    }
    assert!(pre_event, "commands without logged events must also update accounting");
    assert!(versions.contains(&ProtocolVersion::V2));
    assert!(versions.contains(&ProtocolVersion::V4));
    assert!(versions.contains(&ProtocolVersion::LATEST));
}

#[test]
fn replay_refuses_forged_events_before_updating_accounting() {
    let mut replay = Replay::default();
    let entry = LogEntry::Command(crate::log::CommandEntry {
        sequenced: engine_core::command::Sequenced {
            seq: 0,
            ts: 0,
            cmd: crate::test_support::deposit(1, "0xforged-events"),
        },
        events: Some(vec![Event::Deposited { account: crate::test_support::TRADER, amount: Fixed::from_int(100) }]),
    });
    let err = replay.apply(&entry).unwrap_err();
    assert!(err.to_string().contains("events logged at seq 0 differ"), "{err:#}");
    assert_eq!(replay.accounting, Accounting::default());
}

/// Checks each computed command and each outstanding Work against independently summed state.
pub(in crate::sequencer) fn assert_replay(entries: &[LogEntry]) -> Replayed {
    let domain = entries.iter().find_map(|entry| match entry {
        LogEntry::Checkpoint(cp) => cp.domain,
        _ => None,
    });
    let mut replay =
        Replay::with_recovery(domain, Some(AccountId(EnclaveKey::test_key().address())), Default::default());
    let mut oracle = Engine::new();
    let mut unpaid = Fixed::ZERO;
    for entry in entries {
        replay.apply(entry).unwrap();
        if let LogEntry::Command(c) = entry {
            let events = oracle.apply(&c.sequenced).unwrap();
            unpaid += approved(&events);
        }
        assert_matches(replay.accounting, &oracle, unpaid);
        if let LogEntry::Checkpoint(cp) = entry
            && let Some(work) = replay.recovery_work.last().filter(|work| work.checkpoint.seq == cp.seq)
        {
            assert_eq!((work.liabilities, work.deposits, work.coverage), recompute_work(&oracle, unpaid));
        }
    }
    let replayed = replay.finish();
    assert_eq!(replayed.unpaid, unpaid);
    replayed
}

/// Measures authenticated outstanding checkpoint application after replaying the population once.
pub(in crate::sequencer) fn recovery_capture_p50(entries: &[LogEntry], expected: (Fixed, u64)) -> Duration {
    let checkpoint = entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(cp),
            _ => None,
        })
        .unwrap();
    let mut replay =
        Replay::with_recovery(checkpoint.domain, Some(AccountId(EnclaveKey::test_key().address())), Default::default());
    for entry in entries {
        replay.apply(entry).unwrap();
    }
    assert_matches(replay.accounting, &replay.engine, Fixed::ONE);
    let expected_work = recompute_work(&replay.engine, Fixed::ONE);
    assert_eq!((expected_work.0, expected_work.1), expected);
    let entry = LogEntry::Checkpoint(checkpoint.clone());
    let mut samples = Vec::new();
    for index in 0..216 {
        // Each sample resumes the same authenticated tail from its predecessor anchor.
        replay.recovery_parent = checkpoint.parent;
        replay.recovery_work.clear();
        let started = Instant::now();
        replay.apply(black_box(&entry)).unwrap();
        let elapsed = started.elapsed();
        if index >= 16 {
            samples.push(elapsed);
        }
        assert_eq!(replay.recovery_work.len(), 1);
        let captured = &replay.recovery_work[0];
        assert_eq!((captured.liabilities, captured.deposits, captured.coverage), expected_work);
    }
    samples.sort_unstable();
    println!(
        "ACCOUNTING recovery evm_deposits={} capture_p50={:?} capture_p99={:?}",
        expected.1, samples[100], samples[198]
    );
    samples[100]
}
