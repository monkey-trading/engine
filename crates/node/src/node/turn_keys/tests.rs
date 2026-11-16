//! The generations a node's `TurnTaken` waits on: read from the finalised schedule for each turn the
//! log assigns the node's key and has yet to see taken, when the schedule names the node's release.

use super::*;

use std::sync::{Arc, Mutex};

use engine_core::Fixed;
use engine_core::auth::Operator;
use engine_core::eip712::B256;
use engine_core::types::Millis;

use crate::checkpoints::{Observation, ScheduledTurn, VaultState};
use crate::log::Checkpoint;
use crate::sequencer::tests::operator::{recover_turns, turn_log};
use crate::test_support::dev_config;

const RELEASE: Release = Release { launch_digest: [0x11; 48], config_sha256: [0xc1; 32] };

#[tokio::test]
async fn the_generation_of_the_logs_untaken_turn_for_this_key_is_published() {
    let (dir, clock, turn, member) = turn_log("turn-generations");
    let recorded = dev_config().operator().unwrap();
    let seq = recover_turns(&dir, clock, Operator { address: member, ..recorded }, B256::repeat_byte(0x56));
    let (tx, rx) = mpsc::channel(16);
    let _run = tokio::spawn(seq.run(rx));
    let schedule = Schedule(Arc::new(Mutex::new(vec![drawn(turn, RELEASE.id(), 7)])), None);
    let (generations, mut published) = watch::channel(BTreeMap::new());
    let every = Duration::from_millis(50);
    let watching = (RELEASE, BTreeMap::new());
    let _watch = tokio::spawn(watch_turn_generations(schedule, watching, member, tx.clone(), generations, every));
    tokio::time::timeout(Duration::from_secs(5), published.changed()).await.expect("nothing published").unwrap();
    assert_eq!(*published.borrow(), BTreeMap::from([(turn, 7)]));

    // Another key's log assigns it no turn.
    assert_eq!(untaken_turns(&tx, AccountId([0xee; 20])).await.unwrap(), Vec::<u64>::new());
}

#[tokio::test]
async fn only_a_drawn_turn_naming_this_release_has_its_generation_read() {
    let (turn, other) = (20_754, [0x99; 32]);
    let turns = vec![drawn(turn - 1, RELEASE.id(), 5), drawn(turn + 1, RELEASE.id(), 6)];
    let schedule = Schedule(Arc::new(Mutex::new(turns)), None);
    // The schedule has not drawn the turn: the latest at its boundary is the one before.
    assert_eq!(read_generations(&schedule, &RELEASE, &[turn]).await.unwrap(), BTreeMap::new());
    schedule.0.lock().unwrap().push(drawn(turn, other, 7));
    assert_eq!(read_generations(&schedule, &RELEASE, &[turn]).await.unwrap(), BTreeMap::new(), "another release's");
    schedule.0.lock().unwrap().last_mut().unwrap().release = RELEASE.id();
    let read = read_generations(&schedule, &RELEASE, &[turn, turn + 1]).await.unwrap();
    assert_eq!(read, BTreeMap::from([(turn, 7), (turn + 1, 6)]));
}

/// A promoted start publishes the generation its promotion carried into the member's next drawn turn,
/// read at the latest agreed block; a finalised schedule that has yet to reach the promotion holds that
/// turn in its draw's earlier generation, and the watch keeps the start's until finality reaches it or
/// moves past it.
#[tokio::test]
async fn the_watch_keeps_a_starts_carried_generation_while_finality_lags_the_promotion() {
    let (dir, clock, turn, member) = turn_log("turn-generations-carried");
    let recorded = dev_config().operator().unwrap();
    let seq = recover_turns(&dir, clock, Operator { address: member, ..recorded }, B256::repeat_byte(0x56));
    let (tx, rx) = mpsc::channel(16);
    let _run = tokio::spawn(seq.run(rx));
    let (observed, mut observations) = mpsc::unbounded_channel();
    let turns = Arc::new(Mutex::new(vec![drawn(turn, RELEASE.id(), 7)]));
    let schedule = Schedule(turns.clone(), Some(observed));
    let started = BTreeMap::from([(turn - 1, 9), (turn, 9)]);
    let (generations, mut published) = watch::channel(started.clone());
    let every = Duration::from_millis(50);
    let watching = (RELEASE, started);
    let _watch = tokio::spawn(watch_turn_generations(schedule, watching, member, tx.clone(), generations, every));
    // A second read has begun only once the first one's generations were published.
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(5), observations.recv()).await.expect("no read").unwrap();
    }
    assert_eq!(*published.borrow_and_update(), BTreeMap::from([(turn, 9)]), "the lagging draw's generation");
    // A move past the promotion, as the member's release moved, is the finalised schedule's to publish.
    turns.lock().unwrap()[0].generation = 11;
    tokio::time::timeout(Duration::from_secs(5), published.changed()).await.expect("nothing published").unwrap();
    assert_eq!(*published.borrow(), BTreeMap::from([(turn, 11)]));
}

/// The start's generation stands for an untaken turn while the finalised schedule holds the turn in an
/// earlier generation or has not drawn it; one at or past it, the finalised schedule's when it names this
/// release, and none when it names another; and the schedule's alone for a turn the start published none for.
#[test]
fn a_starts_generation_stands_until_the_finalised_schedule_reaches_it() {
    const OTHER: [u8; 32] = [0x99; 32];
    let release = RELEASE.id();
    let started = BTreeMap::from([(4, 9), (6, 9)]);
    type Case = (&'static str, Option<(u64, [u8; 32])>, Option<u64>);
    let cases: [Case; 6] = [
        ("not drawn at finality", None, Some(9)),
        ("the draw before the carry", Some((7, release)), Some(9)),
        ("another's assignment before it", Some((7, OTHER)), Some(9)),
        ("the carry itself", Some((9, release)), Some(9)),
        ("a later move of this release", Some((11, release)), Some(11)),
        ("a later move to another", Some((11, OTHER)), None),
    ];
    for (case, finalised, expected) in cases {
        let drawn = finalised.map(|drawn| BTreeMap::from([(4, drawn)])).unwrap_or_default();
        let generations = published(&drawn, release, &started, &[4]);
        assert_eq!(generations.get(&4).copied(), expected, "{case}");
    }
    // A turn taken since the start is not published, whatever the start published for it.
    assert_eq!(published(&BTreeMap::new(), release, &started, &[]), BTreeMap::new());
    let drawn = BTreeMap::from([(5, (3, release)), (7, (3, OTHER))]);
    assert_eq!(published(&drawn, release, &started, &[5, 7]), BTreeMap::from([(5, 3)]), "no start's generation");
}

// ── Helpers ──

/// A finalised schedule's drawn turns, reporting each observation to the sender when it has one.
struct Schedule(Arc<Mutex<Vec<ScheduledTurn>>>, Option<mpsc::UnboundedSender<()>>);

impl CheckpointChain for Schedule {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        if let Some(observed) = &self.1 {
            let _ = observed.send(());
        }
        let zero = Fixed::ZERO;
        Ok(VaultState {
            height: 1,
            signer: AccountId([0; 20]),
            checkpoint: None,
            balance: zero,
            deposits: 0,
            deposited: zero,
            distributions_paid: zero,
        })
    }

    async fn post(&self, _checkpoint: &Checkpoint) -> anyhow::Result<()> {
        anyhow::bail!("this vault takes no checkpoints")
    }

    async fn scheduled_turn(&self, at: &Observation, now_ms: Millis) -> anyhow::Result<Option<ScheduledTurn>> {
        anyhow::ensure!(at.view == View::Finalized, "the schedule is read at finality");
        let turns = self.0.lock().unwrap();
        Ok(turns.iter().filter(|turn| turn.boundary <= now_ms).max_by_key(|turn| turn.turn).cloned())
    }
}

/// `turn`, drawn for a member running `release` in `generation`.
fn drawn(turn: u64, release: [u8; 32], generation: u64) -> ScheduledTurn {
    ScheduledTurn {
        turn,
        boundary: engine_core::turns::boundary(turn).unwrap(),
        candidate: AccountId([0xc1; 20]),
        admin_key: AccountId([0xb1; 20]),
        release,
        generation,
        set: 1,
        source: None,
    }
}
