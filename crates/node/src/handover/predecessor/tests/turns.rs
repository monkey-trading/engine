//! A daily turn's handover: from the turn's boundary, by both sides' trusted time, the finalised
//! schedule's member takes the keys, sealed under the generation the schedule fixed, while the
//! finalised vault still names the incumbent's release; and only for a turn the incumbent's log
//! accepted.

use super::*;

use engine_core::auth::Operator;
use engine_core::eip712::B256;

use crate::checkpoints::ScheduledTurn;
use crate::sequencer::Request;
use crate::sequencer::tests::operator::{recover_turns, turn_log};
use crate::trusted_time::Sample;

/// The generation the schedule fixed for the turn.
const TURN_GENERATION: u64 = 9;

/// A logged turn: the sequencer whose log accepted it, answering queries, the turn, its member's
/// key, and its boundary.
struct Logged {
    engine: mpsc::Sender<Request>,
    _run: JoinHandle<anyhow::Result<()>>,
    turn: u64,
    key: AccountId,
    boundary: i64,
}

impl Logged {
    /// A sequencer over a log that accepted a turn for its member's key, which it has yet to apply.
    fn start(name: &str) -> Self {
        let (dir, clock, turn, key) = turn_log(name);
        let operator = Operator { address: key, ..dev_config().operator().unwrap() };
        let sequencer = recover_turns(&dir, clock, operator, B256::repeat_byte(1));
        let (engine, requests) = mpsc::channel(16);
        let boundary = engine_core::turns::boundary(turn).unwrap();
        Self { engine, _run: tokio::spawn(sequencer.run(requests)), turn, key, boundary }
    }
}

impl Exchange {
    /// The finalised schedule names the successor's release for `turn` under `key` from `boundary`,
    /// while the finalised vault still names the predecessor's release.
    fn schedule_successor(&self, turn: u64, key: AccountId, boundary: i64) {
        let running = Release { launch_digest: RELEASE, config_sha256: PREDECESSOR_CONFIG };
        *self.current.lock().unwrap() = CurrentRelease { id: running.id(), generation: GENERATION };
        *self.scheduled.lock().unwrap() = Some(ScheduledTurn {
            turn,
            boundary,
            candidate: AccountId([0xb0; 20]),
            admin_key: key,
            release: self.upgrade().id(),
            generation: TURN_GENERATION,
            source: None,
        });
    }

    /// Trusted time on both sides: the predecessor's sample and the successor's have reached `ms`.
    fn reach(&mut self, ms: i64) {
        let at = std::time::Instant::now();
        self.samples.send_replace(Some(Sample { at, earliest_ms: ms, latest_ms: ms + 1_000 }));
        self.reached = Some(ms);
    }
}

#[tokio::test]
async fn from_its_boundary_the_turns_member_takes_the_keys_under_the_schedules_generation() {
    let logged = Logged::start("handover-turn-log");
    let mut exchange = Exchange::start_logged("handover-turn", Some(logged.engine.clone())).await;
    exchange.schedule_successor(logged.turn, logged.key, logged.boundary);
    exchange.reach(logged.boundary);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head.clone()));
    let store = exchange.take().await.unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), TURN_GENERATION));
    assert!(file_for(&exchange.successor_dir, &exchange.upgrade(), TURN_GENERATION).exists());
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
}

#[tokio::test]
async fn a_turn_the_incumbents_log_has_not_accepted_gets_no_keys_and_sequencing_goes_on() {
    let logged = Logged::start("handover-turn-unlogged-log");
    let mut exchange = Exchange::start_logged("handover-turn-unlogged", Some(logged.engine.clone())).await;
    // The schedule names a key the log's assignment does not, as for a turn the engine refused.
    exchange.schedule_successor(logged.turn, AccountId([0xee; 20]), logged.boundary);
    exchange.reach(logged.boundary);
    let err = exchange.take().await.err().expect("keys left for a turn the log refused");
    assert!(format!("{err:#}").contains("has not accepted turn"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped for a refused turn");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");
}

#[tokio::test]
async fn before_its_boundary_the_turns_member_gets_no_keys_and_sequencing_goes_on() {
    let logged = Logged::start("handover-turn-early-log");
    let mut exchange = Exchange::start_logged("handover-turn-early", Some(logged.engine.clone())).await;
    exchange.schedule_successor(logged.turn, logged.key, logged.boundary);
    // Trusted time a second short of the boundary, on both sides.
    exchange.reach(logged.boundary - 1_000);
    let err = exchange.take().await.err().expect("keys left before the boundary");
    assert!(format!("{err:#}").contains("the vault's current release is"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped before the boundary");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");
    assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "the successor sealed keys");
}

#[tokio::test]
async fn once_a_turn_names_another_release_the_vaults_stale_release_gets_no_keys() {
    let logged = Logged::start("handover-turn-stale-log");
    let mut exchange = Exchange::start_logged("handover-turn-stale", Some(logged.engine.clone())).await;
    exchange.schedule_successor(logged.turn, logged.key, logged.boundary);
    // The finalised vault still names the successor's release from an earlier holding, but the turn
    // trusted time has reached names another one.
    *exchange.current.lock().unwrap() = CurrentRelease { id: exchange.upgrade().id(), generation: GENERATION };
    exchange.scheduled.lock().unwrap().as_mut().unwrap().release = [0x77; 32];
    exchange.reach(logged.boundary);
    let err = exchange.take().await.err().expect("keys left for a release the turn does not name");
    assert!(format!("{err:#}").contains("the finalised schedule names release"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped");
}

#[tokio::test]
async fn a_successor_whose_own_time_has_not_reached_the_boundary_seals_nothing() {
    let logged = Logged::start("handover-turn-successor-early-log");
    let mut exchange = Exchange::start_logged("handover-turn-successor-early", Some(logged.engine.clone())).await;
    exchange.schedule_successor(logged.turn, logged.key, logged.boundary);
    exchange.reach(logged.boundary);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let _answer = exchange.answer_drain(Ok(head));
    // The predecessor's time has reached the boundary, the successor's has not.
    exchange.reached = Some(logged.boundary - 1);
    let err = exchange.take().await.err().expect("keys sealed before the successor's time reached the boundary");
    assert!(format!("{err:#}").contains("the vault's current release is"), "{err:#}");
    assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "the successor sealed keys");
    assert!(!exchange.run.is_finished(), "the predecessor ended without a confirmation");
}

#[tokio::test]
async fn a_turns_member_retrying_after_the_stop_is_admitted_once_the_sequencer_is_gone() {
    let Logged { engine, _run: run, turn, key, boundary } = Logged::start("handover-turn-retry-log");
    let mut exchange = Exchange::start_logged("handover-turn-retry", Some(engine.clone())).await;
    exchange.schedule_successor(turn, key, boundary);
    exchange.reach(boundary);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    // The first attempt stops sequencing, then fails on the successor's side.
    exchange.reached = Some(boundary - 1);
    assert!(exchange.take().await.is_err(), "keys sealed before the successor's time reached the boundary");
    // The stopped node's sequencer ends; the listener holds no sender of its own to keep it.
    drop(engine);
    timeout(Duration::from_secs(5), run).await.expect("the sequencer ended").unwrap().unwrap();
    exchange.reached = Some(boundary);
    let store = exchange.take().await.expect("the retry is admitted under the turn the log accepted");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), TURN_GENERATION));
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}
