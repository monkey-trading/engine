//! How the node ends: supervising its tasks until one stops, a shutdown signal arrives, or a
//! handover stops sequencing — which commits only once the sequencer has written the final
//! checkpoint it hands over — then ending history and waiting for the successor to confirm.

use anyhow::{Context, anyhow, bail};
use tokio::signal::ctrl_c;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::alerts::Routing;
use crate::handover::DrainReply;
use crate::node::SHUTDOWN_GRACE;
use crate::sequencer::Request;
use crate::sequencer_thread::Worker;

/// The tasks a node runs as one supervised unit, named by their role.
pub(super) type TaskResult = (&'static str, anyhow::Result<()>);

/// What the node's stop at the sequencer answers for a handover.
enum HandoverStop {
    /// The checkpoint gate admitted the final checkpoint: stop the tasks, and answer the
    /// predecessor with the drained head once the sequencer has ended.
    Committed(DrainReply),
    /// The gate refused it: the predecessor reports the reason and the node goes on sequencing.
    Refused,
}

/// The node's supervised pieces, handed to `supervise` and returned with its outcome.
pub(super) struct Supervised {
    pub tasks: JoinSet<TaskResult>,
    pub history_task: JoinSet<anyhow::Result<()>>,
    pub history_finished: bool,
    pub sequencer: Worker,
    pub handover: JoinSet<anyhow::Result<()>>,
    pub handing_over: Option<DrainReply>,
    /// Why supervision ended: a shutdown signal or a committed handover (`Ok`), or the error.
    pub outcome: anyhow::Result<()>,
}

/// Runs the node's tasks until one stops or panics, a shutdown signal arrives, or an approved
/// handover stops sequencing, and records why in `outcome`. A handover stops the node only once
/// the sequencer has admitted its final checkpoint; refused by the checkpoint gate, the node
/// answers the predecessor's refusal and goes on sequencing.
/// Error cases:
/// - in `outcome`: a supervised task stops or panics, the sequencer stops, the sequencer is gone
///   when a handover asks it to, or the shutdown signal cannot be installed
pub(super) async fn supervise(
    mut supervised: Supervised,
    routing: &mut Routing,
    drain_rx: &mut mpsc::Receiver<DrainReply>,
    handover_stop_tx: &mpsc::Sender<Request>,
) -> Supervised {
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        // `None` is a handover the checkpoint gate refused: the node goes on sequencing.
        let stop = tokio::select! {
            joined = supervised.tasks.join_next() => Some(Err(stopped_task(joined))),
            joined = supervised.history_task.join_next() => {
                supervised.history_finished = true;
                Some(Err(stopped_task(joined.map(|joined| joined.map(|result| ("history", result))))))
            }
            err = routing.stopped() => {
                error!(alert = "task_stopped", "{err:#}");
                Some(Err(err))
            }
            joined = &mut supervised.sequencer.outcome => {
                supervised.tasks.shutdown().await;
                supervised.history_task.abort_all();
                Some(sequencer_outcome(joined).and_then(|()| Err(anyhow!("sequencer stopped"))))
            }
            joined = supervised.handover.join_next(), if !supervised.handover.is_empty() => {
                Some(Err(stopped_task(joined.map(|joined| joined.map(|result| ("handover", result))))))
            }
            Some(reply) = drain_rx.recv() => {
                let probe = handover_stop(handover_stop_tx, reply);
                tokio::pin!(probe);
                // The probe races the shutdown signal: the signal supersedes a probe the sequencer
                // has not answered, and the drain reply drops with the probe, which the
                // predecessor reports as a stop with no final checkpoint.
                tokio::select! {
                    probed = &mut probe => match probed {
                        Ok(HandoverStop::Committed(reply)) => {
                            supervised.handing_over = Some(reply);
                            info!("handing the keys over: stopping");
                            Some(Ok(()))
                        }
                        Ok(HandoverStop::Refused) => None,
                        Err(e) => Some(Err(e)),
                    },
                    signal = &mut shutdown => Some(signal.map(|()| info!("shutting down"))),
                }
            },
            signal = &mut shutdown => Some(signal.map(|()| info!("shutting down"))),
        };
        match stop {
            Some(outcome) => {
                supervised.outcome = outcome;
                return supervised;
            }
            None => info!("the handover was refused: going on sequencing"),
        }
    }
}

/// Stops sequencing for a handover through the sequencer, which writes the final checkpoint the
/// successor is handed: the stop commits only when the checkpoint gate admits one, and is refused
/// with the gate's reason while it would withhold it.
/// Error cases:
/// - the sequencer is gone, or stops before it answers
async fn handover_stop(tx: &mpsc::Sender<Request>, reply: DrainReply) -> anyhow::Result<HandoverStop> {
    let (stop, stopped) = oneshot::channel();
    tx.send(Request::FinalCheckpoint(stop))
        .await
        .map_err(|_| anyhow!("the sequencer stopped before the handover's final checkpoint"))?;
    match stopped.await {
        Ok(Ok(())) => Ok(HandoverStop::Committed(reply)),
        Ok(Err(e)) => {
            let _ = reply.send(Err(e));
            Ok(HandoverStop::Refused)
        }
        Err(_) => Err(anyhow!("the sequencer stopped before the handover's final checkpoint")),
    }
}

/// Resolves on SIGTERM or ctrl-c.
/// Error cases:
/// - a signal handler cannot be installed
async fn shutdown_signal() -> anyhow::Result<()> {
    let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    tokio::select! {
        r = ctrl_c() => r.context("listening for ctrl-c"),
        _ = term.recv() => Ok(()),
    }
}

/// Ends history once the sequencer has drained. Authenticated history takes the rest of the
/// stream; the legacy index takes no captures and resumes from its own cursor, so it stops at once.
pub(super) async fn finish_history(
    mut history: JoinSet<anyhow::Result<()>>,
    stopper: Option<crate::history::runtime::Stopper>,
) {
    match stopper {
        Some(stopper) => drain_history(history, stopper).await,
        None => history.abort_all(),
    }
}

/// Lets history take the rest of the stream, including the shutdown checkpoint's capture. A
/// history still behind after the shutdown grace stops at its next turn boundary, with a Warning,
/// so no store is left mid-transaction; the next start indexes the rest from the log.
async fn drain_history(mut history: JoinSet<anyhow::Result<()>>, stopper: crate::history::runtime::Stopper) {
    let finished = match timeout(SHUTDOWN_GRACE, history.join_next()).await {
        Ok(finished) => finished,
        Err(_) => {
            warn!(alert = "history_drain_timed_out", "history did not finish the stream in time; stopping it");
            stopper.stop();
            match timeout(SHUTDOWN_GRACE, history.join_next()).await {
                Ok(finished) => finished,
                Err(_) => {
                    error!(alert = "task_stopped", "history did not stop in time");
                    history.abort_all();
                    return;
                }
            }
        }
    };
    match finished {
        None | Some(Ok(Ok(()))) => {}
        Some(Ok(Err(e))) => error!(alert = "task_stopped", "history failed while draining: {e:#}"),
        Some(Err(e)) => error!(alert = "task_stopped", "history panicked while draining: {e}"),
    }
}

/// Waits for the handover task once sequencing has stopped: the successor's confirmation ends the
/// node, as does a shutdown signal, with an error.
/// Error cases:
/// - a signal arrives first, or the task fails
pub(super) async fn handed_over(mut handover: JoinSet<anyhow::Result<()>>) -> anyhow::Result<()> {
    tokio::select! {
        joined = handover.join_next() => match joined {
            Some(Ok(result)) => result,
            Some(Err(e)) => Err(anyhow!(e).context("the handover task panicked")),
            None => Err(anyhow!("the handover task is gone")),
        },
        signal = shutdown_signal() => {
            signal?;
            error!(
                alert = "handover_failed",
                "stopped before a successor confirmed the handover: the vault names the successor, so this \
                 release's keys no longer open, and unless a successor holds them it needs new keys, which the \
                 timelock registers"
            );
            bail!("stopped before a successor confirmed the handover")
        }
    }
}

/// The error for a supervised task that stopped, raised as a critical alert.
fn stopped_task(joined: Option<Result<TaskResult, tokio::task::JoinError>>) -> anyhow::Error {
    let err = match joined {
        Some(Ok((name, Ok(())))) => anyhow!("{name} task stopped"),
        Some(Ok((name, Err(e)))) => e.context(format!("{name} task failed")),
        Some(Err(e)) => anyhow!(e).context("task panicked"),
        None => anyhow!("no tasks running"),
    };
    error!(alert = "task_stopped", "{err:#}");
    err
}

/// The sequencer's own result, or its panic.
/// Error cases:
/// - the sequencer panicked, or returned an error (a log write failure)
pub(super) fn sequencer_outcome(joined: Result<anyhow::Result<()>, oneshot::error::RecvError>) -> anyhow::Result<()> {
    joined.context("sequencer thread panicked")?.inspect_err(|e| error!(alert = "sequencer_failed", "{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tee::keys::Head;

    #[tokio::test]
    async fn the_handover_tasks_outcome_is_the_nodes() {
        let mut confirmed = JoinSet::new();
        confirmed.spawn(async { anyhow::Ok(()) });
        handed_over(confirmed).await.expect("a confirmed handover ends the node cleanly");

        let mut unconfirmed = JoinSet::new();
        unconfirmed.spawn(async { Err(anyhow!("the handover was not confirmed")) });
        let err = handed_over(unconfirmed).await.unwrap_err();
        assert!(err.to_string().contains("not confirmed"), "{err:#}");

        let mut panicked = JoinSet::new();
        panicked.spawn(async {
            if true {
                panic!("a bug in the handover task");
            }
            anyhow::Ok(())
        });
        let err = handed_over(panicked).await.unwrap_err();
        assert!(err.to_string().contains("panicked"), "{err:#}");

        let err = handed_over(JoinSet::new()).await.unwrap_err();
        assert!(err.to_string().contains("is gone"), "{err:#}");
    }

    /// A committed stop hands the drain reply back for the answer the node gives once the
    /// sequencer has drained; a refused one answers it with the gate's reason.
    #[tokio::test]
    async fn a_handover_stop_commits_or_refuses_with_the_sequencers_answer() {
        for refused in [false, true] {
            let (tx, mut rx) = mpsc::channel(1);
            let (reply, answered) = oneshot::channel::<anyhow::Result<Head>>();
            let ask = tokio::spawn(async move { handover_stop(&tx, reply).await });
            let Request::FinalCheckpoint(stop) = rx.recv().await.unwrap() else {
                panic!("the stop went to the sequencer as a final-checkpoint request");
            };
            let outcome = if refused {
                Err(anyhow!(crate::sequencer::HandoverRefused::new(
                    "the final checkpoint is withheld (collateral)".into(),
                )))
            } else {
                Ok(())
            };
            stop.send(outcome).unwrap();
            let stop = ask.await.unwrap().unwrap();
            match stop {
                HandoverStop::Committed(committed) => {
                    assert!(!refused, "a gate refusal committed the stop");
                    // The node answers the committed reply once the sequencer has drained.
                    drop(committed);
                    assert!(answered.await.is_err(), "the committed reply is answered later");
                }
                HandoverStop::Refused => {
                    assert!(refused, "an admitted stop came back refused");
                    let err = answered.await.unwrap().unwrap_err();
                    assert!(format!("{err:#}").contains("withheld"), "{err:#}");
                }
            }
        }
    }

    /// The node stops when the sequencer is gone at the handover's stop.
    #[tokio::test]
    async fn a_handover_stop_without_a_sequencer_is_an_error() {
        let (tx, rx) = mpsc::channel::<Request>(1);
        drop(rx);
        let (reply, _answered) = oneshot::channel::<anyhow::Result<Head>>();
        let err = match handover_stop(&tx, reply).await {
            Ok(_) => panic!("a stop without a sequencer committed"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("the sequencer stopped"), "{err:#}");
    }
}
