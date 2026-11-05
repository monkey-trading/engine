//! A daily turn's handover: from the turn's boundary, by both sides' trusted time, the finalised
//! schedule's member takes the keys, sealed under the generation the schedule fixed, while the
//! finalised vault still names the incumbent's release.

use super::*;

use crate::checkpoints::ScheduledTurn;
use crate::trusted_time::Sample;

/// The turn's boundary, and the generation the schedule fixed for it.
const BOUNDARY: i64 = 1_793_226_600_000;
const TURN_GENERATION: u64 = 9;

impl Exchange {
    /// The finalised schedule names the successor's release for a turn starting at `BOUNDARY`, while
    /// the finalised vault still names the predecessor's release.
    fn schedule_successor(&self) {
        let running = Release { launch_digest: RELEASE, config_sha256: PREDECESSOR_CONFIG };
        *self.current.lock().unwrap() = CurrentRelease { id: running.id(), generation: GENERATION };
        *self.scheduled.lock().unwrap() = Some(ScheduledTurn {
            turn: 20_754,
            boundary: BOUNDARY,
            candidate: AccountId([0xb0; 20]),
            admin_key: AccountId([0xb1; 20]),
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
    let mut exchange = Exchange::start("handover-turn").await;
    exchange.schedule_successor();
    exchange.reach(BOUNDARY);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head.clone()));
    let store = exchange.take().await.unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), TURN_GENERATION));
    assert!(file_for(&exchange.successor_dir, &exchange.upgrade(), TURN_GENERATION).exists());
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
}

#[tokio::test]
async fn before_its_boundary_the_turns_member_gets_no_keys_and_sequencing_goes_on() {
    let mut exchange = Exchange::start("handover-turn-early").await;
    exchange.schedule_successor();
    // Trusted time a second short of the boundary, on both sides.
    exchange.reach(BOUNDARY - 1_000);
    let err = exchange.take().await.err().expect("keys left before the boundary");
    assert!(format!("{err:#}").contains("the vault's current release is"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped before the boundary");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");
    assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "the successor sealed keys");
}

#[tokio::test]
async fn a_successor_whose_own_time_has_not_reached_the_boundary_seals_nothing() {
    let mut exchange = Exchange::start("handover-turn-successor-early").await;
    exchange.schedule_successor();
    exchange.reach(BOUNDARY);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let _answer = exchange.answer_drain(Ok(head));
    // The predecessor's time has reached the boundary, the successor's has not.
    exchange.reached = Some(BOUNDARY - 1);
    let err = exchange.take().await.err().expect("keys sealed before the successor's time reached the boundary");
    assert!(format!("{err:#}").contains("the vault's current release is"), "{err:#}");
    assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "the successor sealed keys");
    assert!(!exchange.run.is_finished(), "the predecessor ended without a confirmation");
}
