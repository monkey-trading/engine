//! Logs written under earlier protocol versions, which activate the latest at their next seq and
//! still verify.

use super::*;

use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::params::StressTrigger;

#[test]
fn an_existing_v1_log_activates_at_its_next_seq_and_still_verifies() {
    let dir = scratch_dir("activation-v1");
    fs::create_dir_all(&dir).unwrap();
    // A log written as one file before segments existed continues as the first segment.
    crate::sequencer::fixture_tests::copy_fixture(V1_DEV, dir.join("00000000000000000000.jsonl"));
    let logged = log_entries(&dir);
    let mut carried = replay(&logged).unwrap().engine.params().cloned().expect("the log sets parameters");
    let mut seq = recover_with(&dir, &signed(31_337), finished_genesis(&logged)).unwrap();
    let weighting = dev_config().genesis.params.toxicity.notional.unwrap();
    seq.carry_toxicity_notional(weighting).unwrap();
    seq.activate_protocol().unwrap();
    // The original 63 commands and 3 checkpoints, then the config's notional weighting and its
    // checkpoint, then client auth, the operator, the carried ceilings, and the activation, with
    // the scheduled checkpoint after the operator and the activation's own.
    let replayed = replay(&log_entries(&dir)).unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (68, 6));
    assert_eq!(replayed.activations, [(ProtocolVersion::LATEST, 67)]);
    // Activation also turns its absolute divergence triggers of 15 and 20 bps into 60% and 80% of its
    // widest bound, 25 bps: the shares the dev config takes; its toxicity into `V54`'s form; the default
    // curve; and gives level 2 a house loss threshold of zero.
    carried.toxicity.notional = Some(weighting);
    carried.share_divergence_triggers(25);
    carried.convert_to_score();
    carried.profit_fee_curve = engine_core::holding_fee::DEFAULT_PROFIT_FEE_CURVE.to_vec();
    carried.stress_levels[2].trigger.get_or_insert_with(Default::default).house_loss = Some(Fixed::ZERO);
    assert_eq!(replayed.engine.params(), Some(&carried));
    // Each trigger is the dev config's, but for the house loss threshold, a fixture there.
    let triggers = |p: &ProtocolParams| {
        let without_loss = |trigger: &StressTrigger| StressTrigger { house_loss: None, ..trigger.clone() };
        p.stress_levels.iter().map(|level| level.trigger.as_ref().map(without_loss)).collect::<Vec<_>>()
    };
    assert_eq!(triggers(&carried), triggers(&dev_config().genesis.params));
}

#[test]
fn a_v3_log_takes_the_state_tree_at_its_next_seq_and_still_verifies() {
    let dir = scratch_dir("activation-v3");
    fs::create_dir_all(&dir).unwrap();
    crate::sequencer::fixture_tests::copy_fixture(V3_REFERENCE, dir.join("00000000000000000000.jsonl"));
    let logged = log_entries(&dir);
    let before = replay(&logged).unwrap();
    // As the node does, the log's parameters take the config's notional weighting at its next seq;
    // the operator follows, and the latest version after it.
    let activated = before.engine.last_seq().unwrap() + 4;
    // The log is stamped ahead of the system clock, so the host clock and trusted time agree on a
    // time just after its last command.
    let stamps = Arc::new(TestClock::at(before.engine.now() + 1_000));
    let (stream, _) = broadcast::channel(16);
    let (manifest, _) = watch::channel(Manifest::default());
    let mut operator = dev_config().operator().unwrap();
    operator.vault = AccountId([0xaa; 20]);
    // The V3 log lists markets the dev config never ceilinged; `V40` needs an adopted ceiling for
    // each, so the config this test runs under names one per listed market.
    let ceilings: Vec<_> = before
        .engine
        .markets()
        .keys()
        .map(|market| CloseSpreadCeiling { market: *market, max_close_spread_bps: 30 })
        .collect();
    let cfg = SequencerConfig {
        client_auth: signed(31_337),
        vault: operator.vault,
        operator,
        clock: stamps.clone(),
        genesis: finished_genesis(&logged),
        close_spread_ceilings: ceilings,
        ..sequencer_config(&dir, manifest)
    };
    let key = Arc::new(EnclaveKey::test_key());
    let mut seq = Sequencer::recover(cfg, key, stream, mpsc::unbounded_channel().0).unwrap();
    seq.trust(sample_around(stamps.as_ref())).unwrap();
    seq.carry_toxicity_notional(dev_config().genesis.params.toxicity.notional.unwrap()).unwrap();
    seq.activate_protocol().unwrap();
    // The V3 checkpoints still verify under the hash of the state encoding, and the activation's
    // checkpoint under the state tree, whose proofs hold against it.
    let written = log_entries(&dir);
    let replayed = replay(&written).unwrap();
    assert_eq!(replayed.activations, [(ProtocolVersion::V3, 1), (ProtocolVersion::LATEST, activated)]);
    assert!(replayed.checkpoints > before.checkpoints);
    let Some(LogEntry::Checkpoint(last)) = written.last() else { panic!("the activation is checkpointed") };
    let account = *replayed.engine.accounts().keys().next().unwrap();
    let proof = replayed.engine.account_proof(account).unwrap();
    assert_eq!((proof.seq, proof.verify(last.root.parse().unwrap())), (activated, true));
}
