//! How the node ends: supervising its tasks until one stops, a shutdown signal arrives, or a
//! handover stops sequencing — which commits only once the sequencer has written the final
//! checkpoint it hands over — then answering a handover's successor, ending history, and waiting for the
//! successor to confirm.

use anyhow::{Context, anyhow, bail};
use tokio::signal::ctrl_c;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::alerts::Routing;
use crate::handover::DrainReply;
use crate::handover::assigned::Assigned;
use crate::node::SHUTDOWN_GRACE;
use crate::node::member::Handed;
use crate::sequencer::Request;
use crate::sequencer_thread::Worker;

/// The tasks a node runs as one supervised unit, named by their role.
pub(super) type TaskResult = (&'static str, anyhow::Result<()>);

/// The handover listener's outcome: a confirmed handover, with the assignment its successor bound, if
/// any (`Predecessor::run`).
pub(super) type HandoverResult = anyhow::Result<Option<Assigned>>;

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
    pub handover: JoinSet<HandoverResult>,
    pub handing_over: Option<DrainReply>,
    /// Whether supervision ended because the vault named another key (`liveness::Gate::replace`).
    pub replaced: bool,
    /// Why supervision ended: a shutdown signal, a committed handover or the vault naming another key
    /// (`Ok`), or the error.
    pub outcome: anyhow::Result<()>,
}

/// Runs the node's tasks until one stops or panics, a shutdown signal arrives, an approved handover
/// stops sequencing, or the liveness rule's `replacement` shows the vault naming another key, and
/// records why in `outcome` and `replaced`. A handover stops the node only once the sequencer has
/// admitted its final checkpoint; refused by the checkpoint gate, the node answers the predecessor's
/// refusal and goes on sequencing.
/// Error cases:
/// - in `outcome`: a supervised task stops or panics, the sequencer stops, the sequencer is gone
///   when a handover asks it to, or the shutdown signal cannot be installed
pub(super) async fn supervise(
    mut supervised: Supervised,
    routing: &mut Routing,
    (drain_rx, handover_stop_tx): (&mut mpsc::Receiver<DrainReply>, &mpsc::Sender<Request>),
    mut replacement: Option<watch::Receiver<bool>>,
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
                let joined = joined.map(|joined| joined.map(|result| ("handover", result.map(|_| ()))));
                Some(Err(stopped_task(joined)))
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
            () = replaced(&mut replacement) => {
                supervised.replaced = true;
                warn!("the vault names another key than this enclave's: ending this run");
                Some(Ok(()))
            }
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

/// Resolves once `replacement` shows the vault naming another key; never without one, or once its
/// sender is gone.
async fn replaced(replacement: &mut Option<watch::Receiver<bool>>) {
    if let Some(replacement) = replacement
        && replacement.wait_for(|replaced| *replaced).await.is_ok()
    {
        return;
    }
    std::future::pending().await
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
pub(super) async fn shutdown_signal() -> anyhow::Result<()> {
    let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    tokio::select! {
        r = ctrl_c() => r.context("listening for ctrl-c"),
        _ = term.recv() => Ok(()),
    }
}

/// Starts history's task with its feed and the deriver of each command's derived rows, outside the
/// supervised tasks: on shutdown it outlives the sequencer, so it can take the final checkpoint's
/// capture, and stops at the end of the stream. Its own set still aborts it if the node's run returns
/// early. Returns the set and the handle that stops an authenticated owner (`finish_history`).
pub(super) fn spawn_history(
    history: crate::history::History,
    feed: crate::api::Feed,
    deriver: crate::history::derived::Deriver,
) -> (JoinSet<anyhow::Result<()>>, Option<crate::history::runtime::Stopper>) {
    let stopper = history.stopper();
    let mut task = JoinSet::new();
    task.spawn(history.run(feed, deriver));
    (task, stopper)
}

/// Once the sequencer has drained, answers a committed handover's successor with `final_head`, then
/// ends history (`finish_history`, given the history task unless it has already ended). The successor
/// takes only the final checkpoint the drain wrote, which history does not change, so its take never
/// waits on the derived index. Returns whether a handover was answered.
pub(super) async fn answer_then_finish_history(
    handing_over: Option<DrainReply>,
    final_head: impl FnOnce() -> anyhow::Result<crate::tee::keys::Head>,
    history: Option<JoinSet<anyhow::Result<()>>>,
    stopper: Option<crate::history::runtime::Stopper>,
) -> bool {
    let answered = handing_over.map(|reply| reply.send(final_head())).is_some();
    if let Some(history) = history {
        finish_history(history, stopper).await;
    }
    answered
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

/// Lets history take the rest of the stream, including the shutdown checkpoint's capture, for at most
/// `SHUTDOWN_GRACE`. A history still behind then is stopped, with a Warning, and has `SHUTDOWN_GRACE`
/// more to leave at its next turn boundary, which a stopped owner reaches within one store write; the
/// next start indexes the rest from the log. One that has still not left is aborted and left, with a
/// Critical alert. Leaving it, even mid-write, is safe, since the index is derived from the log: a store
/// transaction cut short is rolled back from its journal when the store next opens, and no signed head
/// names a version an update cut short committed; a cache or signed head becomes visible only by a
/// rename once its file is synced, and a staged one left behind is reclaimed at the next start; and the
/// next start resumes the index from its newest signed head, discards every version committed after it,
/// and indexes the rest again from the log, checking every node and value as it goes.
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
/// node, giving away what `gave` holds and the assignment the successor bound, if any, as does a
/// shutdown signal, with an error.
/// Error cases:
/// - a signal arrives first, or the task fails
pub(super) async fn handed_over(mut handover: JoinSet<HandoverResult>, gave: Handed) -> anyhow::Result<Handed> {
    tokio::select! {
        joined = handover.join_next() => match joined {
            Some(Ok(result)) => result.map(|to| Handed { to, ..gave }),
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

    /// The vault naming another key ends supervision cleanly, marked as a replacement, while the tasks
    /// run on; a node the rule does not run on, with no replacement to watch, is not ended by it.
    #[tokio::test]
    async fn a_vault_naming_another_key_ends_supervision_as_a_replacement() {
        let mut cfg = crate::test_support::dev_config();
        cfg.alerts.outbox.dir = crate::test_support::scratch_dir("supervise-replaced").join("alerts");
        let metrics = std::sync::Arc::new(crate::metrics::Metrics::new());
        let (_layer, alerts) = crate::alerts::channel(crate::alerts::Routes::default(), metrics);
        let mut routing = alerts.spawn(cfg.alerts.episode_quiet, &cfg.alerts.outbox).await.unwrap();
        let (_drain_tx, mut drain_rx) = mpsc::channel(1);
        let (stop_tx, _stop_rx) = mpsc::channel(1);
        let supervised = || {
            let (mut tasks, mut history_task) = (JoinSet::new(), JoinSet::new());
            tasks.spawn(std::future::pending::<TaskResult>());
            history_task.spawn(std::future::pending());
            Supervised {
                tasks,
                history_task,
                history_finished: false,
                sequencer: Worker::start(std::future::pending(), None).unwrap(),
                handover: JoinSet::new(),
                handing_over: None,
                replaced: false,
                outcome: Ok(()),
            }
        };
        let gate = crate::liveness::Gate::closed();
        {
            let watching = supervise(supervised(), &mut routing, (&mut drain_rx, &stop_tx), Some(gate.replacement()));
            tokio::pin!(watching);
            if let Ok(early) = timeout(std::time::Duration::from_millis(200), &mut watching).await {
                panic!("supervision ended while the vault named this key: {:?}", early.outcome);
            }
            gate.replace();
            let ended = timeout(std::time::Duration::from_secs(5), watching).await.expect("the replacement ended it");
            assert!(ended.replaced && ended.outcome.is_ok(), "{:?}", ended.outcome);
        }

        let unwatched = supervise(supervised(), &mut routing, (&mut drain_rx, &stop_tx), None);
        if let Ok(ended) = timeout(std::time::Duration::from_millis(200), unwatched).await {
            panic!("supervision with no replacement to watch ended: {:?}", ended.outcome);
        }
    }

    #[tokio::test]
    async fn the_handover_tasks_outcome_is_the_nodes() {
        let gave = Handed { generation: Some(4), reached: Some(1_000), head: None, to: None };
        let mut confirmed = JoinSet::new();
        let bound = crate::handover::assigned::tests::skip(12, 1, 0x0b, [0x0b; 32], 21);
        confirmed.spawn(async move { anyhow::Ok(Some(bound)) });
        let handed = handed_over(confirmed, gave).await.expect("a confirmed handover ends the node cleanly");
        assert_eq!(handed, Handed { to: Some(bound), ..gave }, "what the node gave away lost the bound assignment");

        let mut unconfirmed = JoinSet::new();
        unconfirmed.spawn(async { Err(anyhow!("the handover was not confirmed")) });
        let err = handed_over(unconfirmed, gave).await.unwrap_err();
        assert!(err.to_string().contains("not confirmed"), "{err:#}");

        let mut panicked = JoinSet::new();
        panicked.spawn(async {
            if true {
                panic!("a bug in the handover task");
            }
            anyhow::Ok(None)
        });
        let err = handed_over(panicked, gave).await.unwrap_err();
        assert!(err.to_string().contains("panicked"), "{err:#}");

        let err = handed_over(JoinSet::new(), gave).await.unwrap_err();
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

    /// Once the sequencer has drained, a handover's successor is answered while history is still
    /// behind. History then has the shutdown grace to finish the stream, and once stopped the grace
    /// again to leave, and no more: one that does neither is aborted, with a Warning and a Critical
    /// alert. A stop with no handover reads no final head.
    #[tokio::test(start_paused = true)]
    async fn a_handover_is_answered_before_history_and_history_waits_two_graces_at_most() {
        let (_guard, mut alerts) = crate::test_support::capture_alerts();
        let (stopper, stopped) = crate::history::runtime::Stopper::detached();
        let mut history = JoinSet::new();
        let behind = history.spawn(std::future::pending::<anyhow::Result<()>>());
        let (reply, answered) = oneshot::channel::<anyhow::Result<Head>>();
        let head = Head { seq: 7, root: format!("0x{}", "07".repeat(32)) };
        let ending = tokio::spawn({
            let head = head.clone();
            async move {
                let began = tokio::time::Instant::now();
                let handed = answer_then_finish_history(Some(reply), || Ok(head), Some(history), Some(stopper)).await;
                (handed, began.elapsed())
            }
        });
        let answer = answered.await.expect("the successor is answered").expect("with the final head");
        assert_eq!(answer, head);
        assert!(!ending.is_finished(), "the successor was answered only once history ended");
        let (handed, waited) = ending.await.unwrap();
        assert!(handed, "the handover was not reported answered");
        let graces = SHUTDOWN_GRACE * 2;
        assert!(
            (graces..graces + std::time::Duration::from_secs(1)).contains(&waited),
            "history was waited on for {waited:?}, not the two graces"
        );
        assert!(stopped.load(std::sync::atomic::Ordering::Acquire), "the history left behind was not told to stop");
        tokio::task::yield_now().await;
        assert!(behind.is_finished(), "the history task was not aborted");
        let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
            .map(|alert| (alert["alert"].clone(), alert["level"].clone()))
            .collect();
        let expected = [("history_drain_timed_out", "WARN"), ("task_stopped", "ERROR")];
        assert_eq!(raised, expected.map(|(alert, level)| (alert.to_owned(), level.to_owned())));

        let handed = answer_then_finish_history(None, || unreachable!("no handover reads no head"), None, None).await;
        assert!(!handed, "a stop with no handover was reported as one");
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
