//! An existing log's parameters, set before `V11`, taking the config's toxicity notional weighting so
//! that the latest version can activate over them.

use super::*;

use engine_core::close_spread::CloseSpreadCeiling;

const V9_REFERENCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v9.jsonl");

#[test]
fn an_existing_log_without_notional_weighting_is_refused_the_latest_version() {
    let dir = scratch_dir("activation-v1-unweighted");
    fs::create_dir_all(&dir).unwrap();
    crate::sequencer::fixture_tests::copy_fixture(V1_DEV, dir.join("00000000000000000000.jsonl"));
    let genesis = finished_genesis(&log_entries(&dir));
    let recover = |dir: &Path, auth: &ClientAuth| recover_with(dir, auth, genesis.clone()).unwrap();
    let err = recover(&dir, &signed(31_337)).activate_protocol().unwrap_err();
    assert!(format!("{err:#}").contains("toxicity needs notional weighting"), "{err:#}");
    // Once its parameters have the weighting, a restart carries nothing more.
    let weighting = dev_config().genesis.params.toxicity.notional.unwrap();
    let mut seq = recover(&dir, &signed(31_337));
    seq.carry_toxicity_notional(weighting).unwrap();
    seq.activate_protocol().unwrap();
    let written = log_entries(&dir).len();
    let mut seq = recover(&dir, &signed(31_337));
    seq.carry_toxicity_notional(weighting).unwrap();
    seq.activate_protocol().unwrap();
    assert_eq!(log_entries(&dir).len(), written);
}

#[test]
fn a_funded_log_under_the_operator_takes_the_weighting_unsigned_and_activates_the_latest_version() {
    let dir = scratch_dir("activation-v9-funded");
    fs::create_dir_all(&dir).unwrap();
    crate::sequencer::fixture_tests::copy_fixture(V9_REFERENCE, dir.join("00000000000000000000.jsonl"));
    let before = replay(&log_entries(&dir)).unwrap();
    // From `V8` an unsigned operator command is refused once funds are credited, as they are here.
    let operator = *before.engine.operator().expect("the log records the operator");
    assert!(before.engine.params().is_some_and(|params| params.toxicity.notional.is_none()));
    // The log is stamped ahead of the system clock, so the host clock and trusted time agree on a
    // time just after its last command.
    let stamps = Arc::new(TestClock::at(before.engine.now() + 1_000));
    let (stream, _) = broadcast::channel(16);
    let (manifest, _) = watch::channel(Manifest::default());
    // The fixture's log lists markets its config never ceilinged; `V40` needs an adopted ceiling
    // for each, so the config this test runs under names one per listed market.
    let ceilings: Vec<_> = before
        .engine
        .markets()
        .keys()
        .map(|market| CloseSpreadCeiling { market: *market, max_close_spread_bps: 30 })
        .collect();
    let cfg = SequencerConfig {
        client_auth: signed(31_337),
        operator,
        vault: operator.vault,
        chain_id: operator.chain_id,
        clock: stamps.clone(),
        genesis: finished_genesis(&log_entries(&dir)),
        close_spread_ceilings: ceilings,
        ..sequencer_config(&dir, manifest)
    };
    let mut seq =
        Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, mpsc::unbounded_channel().0).unwrap();
    seq.trust(sample_around(stamps.as_ref())).unwrap();
    let weighting = dev_config().genesis.params.toxicity.notional.unwrap();
    seq.carry_toxicity_notional(weighting).unwrap();
    seq.activate_protocol().unwrap();
    let replayed = replay(&log_entries(&dir)).unwrap();
    // The weighting, the carried ceilings, and the activation each take one seq.
    let activated = before.engine.last_seq().unwrap() + 3;
    assert_eq!(replayed.activations.last(), Some(&(ProtocolVersion::LATEST, activated)));
    assert_eq!(replayed.engine.params().and_then(|params| params.toxicity.notional), Some(weighting));
}
