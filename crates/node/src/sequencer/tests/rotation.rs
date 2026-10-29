//! The operator key rotates through the sequencer: the old key signs until the notice has run,
//! the new key from then, and the log recovers under the config that names the original key.

use super::*;

use engine_core::eip712;
use engine_core::operator_bounds::NOTICE_MS;
use k256::ecdsa::SigningKey;

use crate::admin::sign;
use crate::clock::Clock;

const START: Millis = 1_792_530_000_000;
const OLD_SEED: u8 = 0x42;
const NEW_SEED: u8 = 0x55;

#[test]
fn a_rotated_key_signs_once_its_notice_has_run_and_the_log_recovers_under_the_original_config() {
    let dir = scratch_dir("operator-rotation");
    let clock = Arc::new(TestClock::at(START));
    let mut seq = recover_at(&dir, clock.clone());
    seq.activate_protocol().unwrap();
    let mut genesis = dev_config().genesis;
    genesis.benchmarks.clear();
    seq.genesis(&genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let new_key = SigningKey::from_slice(&[NEW_SEED; 32]).unwrap();
    let new_address = eip712::address(new_key.verifying_key());

    let reduce_only = |on| AdminCommand::SetReduceOnly { market: None, reduce_only: on };
    let submit = |seq: &mut Sequencer, seed: u8, command| {
        let key = SigningKey::from_slice(&[seed; 32]).unwrap();
        let nonce = seq.engine.next_request_nonce(eip712::address(key.verifying_key()));
        let signed = sign(&key, &seq.operator, command, nonce, clock.now_ms() + 150_000);
        let mut events = seq.sequence(Command::SignedAdmin(Box::new(signed))).unwrap().expect("admitted");
        // A week's rollovers precede the command that follows the advance; its outcome is last.
        events.drain(..events.len() - 1).for_each(drop);
        events
    };

    assert_eq!(
        submit(&mut seq, OLD_SEED, AdminCommand::RotateOperator { address: new_address }),
        [Event::AdminApplied]
    );
    assert_eq!(
        submit(&mut seq, NEW_SEED, reduce_only(true)),
        [Event::AdminRejected { reason: "invalid signature".into() }]
    );
    assert_eq!(submit(&mut seq, OLD_SEED, reduce_only(true)), [Event::AdminApplied]);

    clock.advance(Duration::from_millis(NOTICE_MS as u64 + 1_000));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    assert_eq!(
        submit(&mut seq, OLD_SEED, reduce_only(false)),
        [Event::AdminRejected { reason: "invalid signature".into() }]
    );
    assert_eq!(submit(&mut seq, NEW_SEED, reduce_only(false)), [Event::AdminApplied]);
    assert_eq!(seq.engine.operator().map(|operator| operator.address), Some(new_address));
    seq.flush().unwrap();
    drop(seq);

    // A rotation moves the key only: a config naming another chain or vault is still refused.
    let operator = dev_config().operator().unwrap();
    for other in
        [Operator { chain_id: operator.chain_id + 1, ..operator }, Operator { vault: AccountId([9; 20]), ..operator }]
    {
        let (manifest, _) = watch::channel(Manifest::default());
        let genesis = finished_genesis(&log_entries(&dir));
        let cfg =
            SequencerConfig { clock: clock.clone(), operator: other, genesis, ..sequencer_config(&dir, manifest) };
        let recovered = Sequencer::recover(
            cfg,
            Arc::new(EnclaveKey::test_key()),
            broadcast::channel(16).0,
            mpsc::unbounded_channel().0,
        );
        let err = recovered.err().unwrap();
        assert!(format!("{err:#}").contains("the log records operator"), "{err:#}");
    }
    // The config still names the original key; the log's rotation accounts for the difference.
    let recovered = recover_at(&dir, clock.clone());
    assert_eq!(recovered.engine.operator().map(|operator| operator.address), Some(new_address));
    assert_eq!(recovered.engine.operator_bounds().rotations, 1);
    let replayed = replay(&log_entries(&dir)).unwrap();
    assert_eq!(replayed.engine.operator().map(|operator| operator.address), Some(new_address));
}

fn recover_at(dir: &Path, clock: Arc<TestClock>) -> Sequencer {
    let genesis = finished_genesis(&log_entries(dir));
    let cfg = SequencerConfig { clock, genesis, ..sequencer_config(dir, watch::channel(Manifest::default()).0) };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, mpsc::unbounded_channel().0)
        .unwrap()
}
