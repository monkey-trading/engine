//! A capture's engine comes from the stats worker's copy when one runs, and is encoded on the
//! sequencer's thread otherwise: the same bytes either way.

use std::sync::Arc;

use engine_core::command::Command;
use tokio::sync::{broadcast, mpsc, watch};

use crate::clock::test_clock::TestClock;
use crate::log::Manifest;
use crate::sequencer::{Sequencer, SequencerConfig};
use crate::snapshot::sealing::Encoding;
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, sample_around, scratch_dir, sequencer_config, stats};

#[test]
fn a_capture_takes_the_engine_from_the_stats_worker_when_one_runs() {
    let dir = scratch_dir("snapshot-encoding");
    let clock = Arc::new(TestClock::at(1_792_504_800_000));
    let cfg = SequencerConfig { clock: clock.clone(), ..sequencer_config(&dir, watch::channel(Manifest::default()).0) };
    let key = Arc::new(EnclaveKey::test_key());
    let mut seq = Sequencer::recover(cfg, key, broadcast::channel(32).0, mpsc::unbounded_channel().0).unwrap();
    seq.activate_legacy_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    // Genesis is sequenced once trusted time admits a stamp.
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("trusted time admits the tick");
    // No worker yet: the sequencer encodes the engine itself.
    let (Encoding::Encoded(inline), None) = seq.encoding(seq.last_seq().unwrap()).unwrap() else {
        panic!("with no stats worker the sequencer encodes the engine");
    };
    assert_eq!(inline, serde_json::to_string(&seq.engine).unwrap());
    seq.track_stats(stats());
    seq.sequence(Command::Tick).unwrap().expect("trusted time admits the tick");
    let expected = serde_json::to_string(&seq.engine).unwrap();
    let (Encoding::Pending(reply), Some(job)) = seq.encoding(seq.last_seq().unwrap()).unwrap() else {
        panic!("with a stats worker its copy encodes the engine");
    };
    seq.marks.stats().unwrap().stage_job(job);
    // The command after the checkpoint reaches the worker behind its job, with the batch once durable.
    seq.sequence(Command::Tick).unwrap().expect("trusted time admits the tick");
    seq.flush().unwrap();
    assert_eq!(reply.recv().unwrap(), Ok(expected));
}
