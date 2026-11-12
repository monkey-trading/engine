//! A turn's `TurnTaken` with sealed keys: it waits for the generation the finalised schedule holds for
//! the turn, and seals the keys there first when the schedule names this release in a new one, as it
//! does for a holder returning after a turn no one took, so a restart in the turn opens them.

use super::*;

use engine_core::auth::Operator;
use engine_core::eip712::B256;

use crate::sequencer::tests::operator::{recover_turns, turn_log};
use crate::tee::keys::KeySet;
use crate::tee::sealing::MockSealer;
use crate::test_support::{dev_config, sample_around, scratch_dir};

const DIGEST: [u8; 48] = [0x11; 48];
const CONFIG: [u8; 32] = [0xc1; 32];
/// The generation the keys are sealed in from the turn this node held last.
const HELD: u64 = 3;
/// The fresh generation the schedule gives this node's next turn, after a turn its member never took.
const RETURNING: u64 = 6;

#[test]
fn a_returning_holder_seals_its_keys_in_the_turns_generation_before_taking_it() {
    let Holding { mut seq, turn, store, generations, keys_dir, .. } = holding("turn-reseal");
    // Until the schedule's generation of the turn is read, the turn waits.
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert_eq!(seq.engine.daily_turns().taken, None, "the turn was taken before its generation was read");

    generations.send_replace(BTreeMap::from([(turn, RETURNING)]));
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert_eq!(seq.engine.daily_turns().taken.map(|taken| taken.turn), Some(turn), "the turn was not taken");
    assert_eq!(store.generation(), RETURNING, "the keys were not sealed in the turn's generation");
    // A restart in the turn opens the same keys in its generation.
    let sealer = MockSealer { launch_digest: DIGEST };
    let reopened =
        KeyStore::open(&keys_dir, &sealer, CONFIG, RETURNING).unwrap().expect("the keys in the turn's generation");
    assert_eq!(reopened.keys(), store.keys());
}

#[test]
fn a_continuing_holder_takes_its_turn_in_the_generation_it_holds() {
    let Holding { mut seq, turn, store, generations, keys_dir, .. } = holding("turn-continuing");
    generations.send_replace(BTreeMap::from([(turn, HELD)]));
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert_eq!(seq.engine.daily_turns().taken.map(|taken| taken.turn), Some(turn), "the turn was not taken");
    assert_eq!(store.generation(), HELD);
    assert_eq!(std::fs::read_dir(&keys_dir).unwrap().count(), 1, "the keys were sealed again");
}

#[test]
fn a_turn_whose_keys_cannot_be_sealed_in_its_generation_waits_and_retries_later() {
    let Holding { mut seq, clock, turn, store, generations, keys_dir } = holding("turn-reseal-fails");
    // A directory where the turn's generation's partial file goes makes the write fail.
    let partial = crate::tee::sealing::file_for(&keys_dir, &store.release(), RETURNING).with_extension("partial");
    std::fs::create_dir(&partial).unwrap();
    generations.send_replace(BTreeMap::from([(turn, RETURNING)]));
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert_eq!(seq.engine.daily_turns().taken, None, "the turn was taken with its keys unsealed in its generation");
    assert_eq!(store.generation(), HELD);

    // The disk recovers: the next command within the retry interval does not try again, a later one does.
    std::fs::remove_dir(&partial).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert_eq!(seq.engine.daily_turns().taken, None, "the seal was retried on the next command");
    clock.advance(std::time::Duration::from_millis(RESEAL_RETRY_MS as u64));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert_eq!(seq.engine.daily_turns().taken.map(|taken| taken.turn), Some(turn), "the seal was not retried");
    assert_eq!(store.generation(), RETURNING);
}

// ── Helpers ──

/// A node holding keys sealed in `HELD`, with trusted time, over a log that assigns its key a turn
/// it has reached.
struct Holding {
    seq: Sequencer,
    clock: Arc<crate::clock::test_clock::TestClock>,
    turn: u64,
    store: Arc<KeyStore>,
    /// The schedule's generations of this node's turns, as the sequencer reads them.
    generations: watch::Sender<BTreeMap<u64, u64>>,
    /// Where the keys are sealed.
    keys_dir: std::path::PathBuf,
}

/// `Holding`, in scratch directories for `name`.
fn holding(name: &str) -> Holding {
    let (dir, clock, turn, member) = turn_log(name);
    let keys_dir = scratch_dir(&format!("{name}-keys"));
    let sealer = MockSealer { launch_digest: DIGEST };
    let store = Arc::new(KeyStore::create(&keys_dir, &sealer, CONFIG, HELD, KeySet::generate().unwrap()).unwrap());
    let (generations, published) = watch::channel(BTreeMap::new());
    let recorded = dev_config().operator().unwrap();
    let mut seq = recover_turns(&dir, clock.clone(), Operator { address: member, ..recorded }, B256::repeat_byte(0x55));
    seq.turn_sealing = Some(TurnSealing::new(store.clone(), published));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    Holding { seq, clock, turn, store, generations, keys_dir }
}
