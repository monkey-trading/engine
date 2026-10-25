//! Publisher synchronization refuses bootstrap commands that could charge legacy rollovers.

use super::*;

const LEGACY_START: Millis = 1_792_504_800_000;
const AFTER_OUTAGE: Millis = LEGACY_START + 3 * 86_400_000;

#[test]
fn benchmark_sync_refuses_each_missing_legacy_prerequisite_without_appending() {
    for recorded in 0..8 {
        let dir = scratch_dir("benchmark-prerequisites");
        let mut seq = legacy_history(&dir, recorded, ProtocolVersion::V2);
        assert!(seq.engine.next_rollover().unwrap() < AFTER_OUTAGE);
        let before = (seq.engine.last_seq(), seq.engine.next_rollover(), seq.engine.state_root());
        let written = commands(&log_entries(&dir)).len();
        let result = seq.wait_for_benchmarks();
        if recorded == 7 {
            result.unwrap();
            assert!(seq.benchmark_sync);
        } else {
            let err = result.unwrap_err();
            let prerequisite = if recorded & 1 == 0 {
                "client auth"
            } else if recorded & 2 == 0 {
                "operator"
            } else {
                "toxicity notional weighting"
            };
            assert!(err.to_string().contains(prerequisite), "{recorded}: {err:#}");
            assert!(!seq.benchmark_sync);
        }
        assert_eq!((seq.engine.last_seq(), seq.engine.next_rollover(), seq.engine.state_root()), before);
        assert!(seq.required.is_empty());
        assert_eq!(commands(&log_entries(&dir)).len(), written);
    }
}

#[test]
fn benchmark_sync_allows_cold_genesis_and_a_fully_initialized_v17_log() {
    let dir = scratch_dir("benchmark-cold-genesis");
    let mut cold = recover_at(&dir, LEGACY_START);
    assert!(cold.engine.next_rollover().is_none());
    cold.wait_for_benchmarks().unwrap();
    assert!(cold.benchmark_sync);
    assert!(commands(&log_entries(&dir)).is_empty());

    let dir = scratch_dir("benchmark-v17-history");
    let mut seq = legacy_history(&dir, 7, ProtocolVersion::V17);
    assert_eq!(seq.engine.protocol_version(), ProtocolVersion::V17);
    let written = commands(&log_entries(&dir)).len();
    seq.wait_for_benchmarks().unwrap();
    assert!(seq.benchmark_sync);
    assert_eq!(commands(&log_entries(&dir)).len(), written);
}

/// Writes a valid older log, then recovers it after several missed rollovers.
fn legacy_history(dir: &Path, recorded: u8, version: ProtocolVersion) -> Sequencer {
    let mut seq = recover_at(dir, LEGACY_START);
    let mut params = dev_config().genesis.params;
    let weighting = params.toxicity.notional.take().unwrap();
    for trigger in params.stress_levels.iter_mut().filter_map(|level| level.trigger.as_mut()) {
        trigger.divergence_share_bps = None;
    }
    let events = seq.stamp(LEGACY_START, Command::Admin(AdminCommand::SetParams(params))).unwrap();
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    seq.stamp(LEGACY_START, Command::HouseCapital { amount: Fixed::ONE, chain_ref: "genesis".into() }).unwrap();
    assert!(seq.engine.is_credited("genesis"));
    seq.checkpoint().unwrap();
    let mut setup = Vec::new();
    if recorded & 1 != 0 {
        setup.push(AdminCommand::SetClientAuth(ClientAuth::Unsigned));
    }
    if recorded & 2 != 0 {
        setup.push(AdminCommand::SetOperator(seq.operator));
    }
    if recorded & 4 != 0 {
        setup.push(AdminCommand::SetToxicityNotional(weighting));
    }
    if version > ProtocolVersion::V2 {
        setup.push(AdminCommand::ActivateProtocol { version });
    }
    for command in setup {
        let events = seq.stamp(LEGACY_START, Command::Admin(command)).unwrap();
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
    }
    seq.flush().unwrap();
    drop(seq);
    recover_at(dir, AFTER_OUTAGE)
}

/// Recovers at a controlled host time without sequencing startup prerequisites.
fn recover_at(dir: &Path, at: Millis) -> Sequencer {
    let (stream, _) = broadcast::channel(16);
    let (manifest, _) = watch::channel(Manifest::default());
    let genesis = finished_genesis(&log_entries(dir));
    let cfg = SequencerConfig { clock: Arc::new(TestClock::at(at)), genesis, ..sequencer_config(dir, manifest) };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, mpsc::unbounded_channel().0).unwrap()
}
