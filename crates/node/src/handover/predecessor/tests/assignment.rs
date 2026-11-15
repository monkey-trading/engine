//! A turn's handover bound to the latest agreed block's assignment (#596): a skip's member takes the
//! keys before the finalised schedule shows the skip, under the skip's generation, once the holder's
//! log shows the skip's set naming it with notice; a skip or a promotion at each phase of a transfer —
//! at the admission, before the stop, after the stop before the keys go, after the keys go before the
//! confirmation — leaves the keys with the holder, which goes on serving the member now named; and a
//! promotion racing the boundary leaves the drawn member without them.

use super::*;

use engine_core::auth::Operator;
use engine_core::eip712::B256;

use crate::handover::assigned::tests::skip;
use crate::handover::assigned::{Assigned, Reason};
use crate::sequencer::Request;
use crate::sequencer::tests::operator::{recover_turns, turn_log};

/// The generation the drawn turn holds, a skip's, and a later skip's.
const DRAWN: u64 = 9;
const SKIPPED: u64 = 11;
const LATER: u64 = 14;

/// A sequencer over a log that accepted a turn for its member's key under set 1, which names candidate
/// 0xc1 with that key: the sequencer's channel, its run, the turn, the key and the boundary.
fn logged(name: &str) -> (mpsc::Sender<Request>, JoinHandle<anyhow::Result<()>>, u64, AccountId, i64) {
    let (dir, clock, turn, key) = turn_log(name);
    let operator = Operator { address: key, ..dev_config().operator().unwrap() };
    let sequencer = recover_turns(&dir, clock, operator, B256::repeat_byte(1));
    let (engine, requests) = mpsc::channel(16);
    let boundary = engine_core::turns::boundary(turn).unwrap();
    (engine, tokio::spawn(sequencer.run(requests)), turn, key, boundary)
}

impl Exchange {
    /// The finalised schedule draws `turn` for another member, running another release, with `key`.
    fn draw_other(&self, turn: u64, key: AccountId, boundary: i64) {
        self.schedule_successor(turn, key, boundary);
        let mut scheduled = self.scheduled.lock().unwrap();
        let drawn = scheduled.as_mut().unwrap();
        (drawn.release, drawn.generation, drawn.candidate) = ([0x66; 32], DRAWN, AccountId([0xb0; 20]));
    }

    /// A skip of `turn` at `position` in `generation` to the set's candidate 0xc1 with `key`, running
    /// the successor's release.
    fn skip_to_successor(&self, turn: u64, key: AccountId, position: u16, generation: u64) -> Assigned {
        Assigned { admin_key: key, ..skip(turn, position, 0xc1, self.upgrade().id(), generation) }
    }

    /// The successor's side, binding the latest assignment the shared reader shows.
    async fn take_bound(&self) -> anyhow::Result<KeyStore> {
        let vault = Vault {
            signer: AccountId(self.old.enclave_key().unwrap().address()),
            current: self.current.clone(),
            synced: self.synced.clone(),
            rotation: self.rotation.clone(),
            scheduled: self.scheduled.clone(),
        };
        let sealer = MockSealer { launch_digest: UPGRADE };
        let assignments: Arc<dyn crate::handover::assigned::Assignments> = self.assignments.clone();
        let parts = (&sealer as &dyn crate::tee::sealing::Sealer, &MockAttestor as &dyn crate::tee::Attestor);
        crate::handover::take_with(
            &self.successor,
            &self.address,
            parts,
            &vault,
            self.reached,
            &self.metrics,
            Some(assignments),
        )
        .await
    }
}

/// Another member's assignment of `turn`: a skip past the successor, or a promotion.
fn elsewhere(turn: u64, reason: Reason) -> Assigned {
    Assigned { reason, ..skip(turn, 2, 0xd1, [0x77; 32], 13) }
}

#[tokio::test]
async fn a_skips_member_takes_the_keys_under_the_skip_before_the_finalised_schedule_shows_it() {
    let (engine, _run, turn, key, boundary) = logged("handover-skip-log");
    let mut exchange = Exchange::start_logged("handover-skip", Some(engine.clone())).await;
    exchange.draw_other(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    exchange.assignments.set(Some(exchange.skip_to_successor(turn, key, 1, SKIPPED)));
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    let store = exchange.take_bound().await.expect("the skip's member took the keys");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), SKIPPED), "sealed in the skip's generation");
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
    assert_eq!(exchange.assignments.reads.load(Ordering::SeqCst), 5, "each side read it before each step");
}

#[tokio::test]
async fn a_successor_binding_no_assignment_or_a_skip_its_log_refuses_gets_no_keys() {
    let (engine, _run, turn, key, boundary) = logged("handover-skip-refused-log");
    let mut exchange = Exchange::start_logged("handover-skip-refused", Some(engine.clone())).await;
    exchange.draw_other(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    // An old successor binds nothing while the holder reads a turn's assignment.
    exchange.assignments.set(Some(exchange.skip_to_successor(turn, key, 1, SKIPPED)));
    let err = exchange.take().await.err().expect("keys left for a request binding no assignment");
    assert!(format!("{err:#}").contains("the successor binds None"), "{err:#}");
    // A skip whose key the log's set does not name.
    let unnoticed = Assigned { admin_key: AccountId([0xee; 20]), ..exchange.skip_to_successor(turn, key, 1, SKIPPED) };
    exchange.assignments.set(Some(unnoticed));
    let err = exchange.take_bound().await.err().expect("keys left for a skip the log refuses");
    assert!(format!("{err:#}").contains("refuses turn"), "{err:#}");
    // A promotion naming the successor passes only the finalised checks a draw does, which the drawn
    // assignment the finalised schedule shows fails.
    let promoted = Assigned { reason: Reason::Promotion, ..exchange.skip_to_successor(turn, key, 1, SKIPPED) };
    exchange.assignments.set(Some(promoted));
    let err = exchange.take_bound().await.err().expect("keys left for an unfinal promotion");
    assert!(format!("{err:#}").contains("the finalised schedule names release"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");
    assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "the successor sealed keys");
}

/// A skip past the successor, or a promotion, landing at the admission, between the admission and the
/// stop, after the stop before the keys go, or after the keys go before the confirmation: the keys stay
/// with the holder, which stops only for the later two, and then admits the member named next, with one
/// stop in all.
#[tokio::test]
async fn a_skip_or_a_promotion_at_each_phase_of_a_transfer_leaves_the_keys_with_the_holder() {
    let phases = [("at the admission", 1, false), ("before the stop", 2, false), ("after the stop", 3, true)];
    let phases = phases.into_iter().chain([("before the confirmation", 4, true)]);
    for (phase, reads_before, stops) in phases {
        for reason in [Reason::Skip, Reason::Promotion] {
            let what = format!("{reason:?} {phase}");
            let name = format!("handover-phase-{}-{}", reads_before, reason == Reason::Skip);
            let (engine, _run, turn, key, boundary) = logged(&format!("{name}-log"));
            let mut exchange = Exchange::start_logged(&name, Some(engine.clone())).await;
            exchange.draw_other(turn, key, boundary);
            exchange.reach(boundary + 600_000);
            let bound = exchange.skip_to_successor(turn, key, 1, SKIPPED);
            let moved = elsewhere(turn, reason);
            exchange.assignments.then((0..reads_before).map(|_| Some(bound)).chain([Some(moved)]));
            let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
            // A holder that does not stop never asks for its final checkpoint.
            let answer = stops.then(|| exchange.answer_drain(Ok(head.clone())));
            let err =
                exchange.take_bound().await.err().unwrap_or_else(|| panic!("{what}: the successor took the keys"));
            assert!(format!("{err:#}").contains("moved") || format!("{err:#}").contains("binds"), "{what}: {err:#}");
            if !stops {
                assert!(exchange.drains.try_recv().is_err(), "{what}: the holder stopped");
            }
            assert!(
                !file_for(&exchange.successor_dir, &exchange.upgrade(), SKIPPED).exists(),
                "{what}: the successor sealed the keys"
            );
            assert!(!exchange.run.is_finished(), "{what}: the predecessor stopped serving");
            // The member named next is admitted: here the successor named once more, by a later skip.
            let answer = answer.unwrap_or_else(|| exchange.answer_drain(Ok(head)));
            exchange.assignments.set(Some(exchange.skip_to_successor(turn, key, 3, LATER)));
            let store = exchange.take_bound().await.unwrap_or_else(|e| panic!("{what}: the member now named: {e:#}"));
            assert_eq!(store.generation(), LATER, "{what}");
            timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
            assert_eq!(answer.await.unwrap(), 1, "{what}: sequencing stopped once");
        }
    }
}

/// A promotion that lands as the drawn member takes its turn at the boundary: the holder, reading the
/// promotion as the turn's assignment, refuses the drawn member and goes on sequencing.
#[tokio::test]
async fn a_promotion_racing_the_boundary_leaves_the_drawn_member_without_the_keys() {
    let (engine, _run, turn, key, boundary) = logged("handover-boundary-promotion-log");
    let mut exchange = Exchange::start_logged("handover-boundary-promotion", Some(engine.clone())).await;
    exchange.schedule_successor(turn, key, boundary);
    exchange.reach(boundary);
    let drawn = Assigned { reason: Reason::Draw, ..exchange.skip_to_successor(turn, key, 0, 9) };
    let promoted = Assigned { reason: Reason::Promotion, ..skip(turn, 1, 0xd1, [0x77; 32], 13) };
    exchange.assignments.then([Some(drawn), Some(promoted)]);
    let err = exchange.take_bound().await.err().expect("the drawn member took the keys past a promotion");
    assert!(format!("{err:#}").contains("binds"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped for a member a promotion passed");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");
}

#[tokio::test]
async fn a_skip_before_the_stop_lets_sequencing_go_on() {
    let (engine, _run, turn, key, boundary) = logged("handover-skip-early-log");
    let mut exchange = Exchange::start_logged("handover-skip-early", Some(engine.clone())).await;
    exchange.draw_other(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    let bound = exchange.skip_to_successor(turn, key, 1, SKIPPED);
    exchange.assignments.then([Some(bound), Some(elsewhere(turn, Reason::Skip))]);
    assert!(exchange.take_bound().await.is_err(), "the successor took the keys");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped for a member a skip passed");
}

#[tokio::test]
async fn a_stopped_holder_admits_a_later_turns_skip_only_within_the_lookback() {
    let (engine, run, turn, key, boundary) = logged("handover-skip-lookback-log");
    let mut exchange = Exchange::start_logged("handover-skip-lookback", Some(engine.clone())).await;
    exchange.draw_other(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    // The stop goes through, then the transfer fails on the successor's side, and the sequencer ends.
    let bound = exchange.skip_to_successor(turn, key, 1, SKIPPED);
    let moved = Some(elsewhere(turn, Reason::Skip));
    exchange.assignments.then([Some(bound), Some(bound), Some(bound), Some(bound), moved]);
    assert!(exchange.take_bound().await.is_err());
    drop(engine);
    timeout(Duration::from_secs(5), run).await.expect("the sequencer ended").unwrap().unwrap();
    // Eight turns on, past the lookback, a skip's member is refused; seven on, admitted under the turns
    // the stop retained.
    let far = turn + 8;
    exchange.reach(engine_core::turns::boundary(far).unwrap() + 600_000);
    exchange.assignments.set(Some(exchange.skip_to_successor(far, key, 1, LATER)));
    let err = exchange.take_bound().await.err().expect("keys left past the lookback");
    assert!(format!("{err:#}").contains("more than 7 turns past"), "{err:#}");
    let near = turn + 7;
    exchange.reach(engine_core::turns::boundary(near).unwrap() + 600_000);
    exchange.assignments.set(Some(exchange.skip_to_successor(near, key, 1, LATER)));
    let store = exchange.take_bound().await.expect("a skip within the lookback is admitted");
    assert_eq!(store.generation(), LATER);
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

#[tokio::test]
async fn a_holder_whose_skip_was_reorged_away_hands_the_keys_to_the_drawn_member() {
    // The latest agreed block shows the turn's draw again: the holder the skip named admits the drawn
    // member under the finalised schedule's checks, binding the draw.
    let (engine, _run, turn, key, boundary) = logged("handover-unskipped-log");
    let mut exchange = Exchange::start_logged("handover-unskipped", Some(engine.clone())).await;
    exchange.schedule_successor(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    let drawn = Assigned { reason: Reason::Draw, ..exchange.skip_to_successor(turn, key, 0, DRAWN) };
    exchange.assignments.set(Some(drawn));
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let _answer = exchange.answer_drain(Ok(head));
    let store = exchange.take_bound().await.expect("the drawn member took the keys");
    assert_eq!(store.generation(), DRAWN);
}
