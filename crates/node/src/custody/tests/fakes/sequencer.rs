//! In-memory sequencer stand-ins for watcher tests: no log, no trusted time.

use engine_core::Engine;
use engine_core::command::Sequenced;
use tokio::sync::mpsc;

use crate::sequencer::Request;

/// A sequencer over an in-memory engine that answers sync requests only when `answer_syncs`,
/// as when its fsync fails otherwise.
pub(crate) fn engine_sequencer(answer_syncs: bool) -> mpsc::Sender<Request> {
    refusing_sequencer(answer_syncs, 0)
}

/// `engine_sequencer` that also drops its first `refusals` commands unapplied, with their
/// replies, and keeps running, as the node's sequencer does while trusted time refuses stamps.
pub(crate) fn refusing_sequencer(answer_syncs: bool, refusals: usize) -> mpsc::Sender<Request> {
    let (tx, mut rx) = mpsc::channel::<Request>(64);
    tokio::spawn(async move {
        let (mut engine, mut seq, mut refusals) = (Engine::new(), 0, refusals);
        while let Some(request) = rx.recv().await {
            match request {
                Request::Submit { .. } if refusals > 0 => refusals -= 1,
                Request::Submit { cmd, reply } => {
                    let events = engine.apply(&Sequenced { seq, ts: 0, cmd }).unwrap();
                    seq += 1;
                    if let Some(reply) = reply {
                        let _ = reply.send(events);
                    }
                }
                Request::Query(query) => query(&engine),
                Request::Sync(reply) if answer_syncs => {
                    let _ = reply.send(engine.last_seq());
                }
                Request::Sync(_) => {}
                Request::CheckpointStatus { .. } => unreachable!("no checkpoint worker in this test"),
                Request::WatchBenchmarkSchedule(_) => unreachable!("only the API watches benchmark schedules"),
                Request::HouseState(_) => unreachable!("only the API reads the house state"),
                Request::ReissueWithdrawal { .. } => unreachable!("only the API asks for a reissue"),
                Request::AccountPayouts { .. }
                | Request::EntitlementClaim { .. }
                | Request::DistributionClaim { .. } => {
                    unreachable!("only the API asks for payouts and claims")
                }
                Request::FinalCheckpoint(_) | Request::PrefixAt { .. } => {
                    unreachable!("only a handover asks for the final checkpoint or a tip")
                }
                Request::Freeze => unreachable!("only the log-head poster freezes sequencing"),
                Request::Quote { .. } => unreachable!("custody sends no quotes"),
                Request::TimeSample(_) => unreachable!("custody sends no time samples"),
            }
        }
    });
    tx
}
