//! What follows the sequencer's drain: ending history, at shutdown or a handover, and waiting for
//! the successor to confirm a handover.

use anyhow::{anyhow, bail};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::{error, warn};

use crate::node::{SHUTDOWN_GRACE, shutdown_signal};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
