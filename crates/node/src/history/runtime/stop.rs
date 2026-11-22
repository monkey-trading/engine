//! How the history owner stops: on request, when its task is dropped, and the drain that ends its input.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use tokio::sync::{mpsc, oneshot};

use crate::history::runtime::{Input, Stopper};

/// Cancellation wakes an idle owner and a full queue already guarantees a pending wakeup.
pub(super) struct Stop {
    pub input: mpsc::Sender<Input>,
    pub stopped: Arc<AtomicBool>,
}

impl Drop for Stop {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.input.try_send(Input::Stop);
    }
}

impl Stopper {
    /// Stops the owner after its current turn; `History::run` then returns without an error.
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.input.try_send(Input::Stop);
    }

    /// A stopper of no owner, with the flag `stop` sets.
    #[cfg(test)]
    pub(crate) fn detached() -> (Self, Arc<AtomicBool>) {
        let stopped = Arc::new(AtomicBool::new(false));
        (Self { input: mpsc::channel(1).0, stopped: stopped.clone() }, stopped)
    }
}

/// Asks the owner to take the rest of its input, and waits for it. A requested stop may end the owner
/// before it answers, which is no failure.
/// Error cases:
/// - the owner ended unasked before it answered
pub(super) async fn drain(input: &mpsc::Sender<Input>, stopped: &AtomicBool) -> Result<()> {
    let (drained, done) = oneshot::channel();
    if input.send(Input::Drain(drained)).await.is_ok() && done.await.is_err() {
        ensure!(stopped.load(Ordering::Acquire), "history owner stopped before draining its physical input");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drain waits for the owner's answer. An owner that ends unasked before it answers fails it, one
    /// a requested stop ended does not, and an owner already gone leaves nothing to drain.
    #[tokio::test]
    async fn a_drain_left_unanswered_by_a_requested_stop_is_no_failure() {
        let stopped = AtomicBool::new(false);
        let (input, mut owner) = mpsc::channel(1);
        let answering = tokio::spawn(async move {
            let Some(Input::Drain(done)) = owner.recv().await else { panic!("the owner was not asked to drain") };
            done.send(()).unwrap();
        });
        drain(&input, &stopped).await.expect("an answered drain");
        answering.await.unwrap();

        let (input, mut owner) = mpsc::channel(1);
        let ending = tokio::spawn(async move { drop(owner.recv().await) });
        let failed = drain(&input, &stopped).await.expect_err("an owner that ended unasked");
        assert!(format!("{failed:#}").contains("before draining"), "{failed:#}");
        ending.await.unwrap();

        stopped.store(true, Ordering::Release);
        let (input, mut owner) = mpsc::channel(1);
        let ending = tokio::spawn(async move { drop(owner.recv().await) });
        drain(&input, &stopped).await.expect("a drain a requested stop left unanswered");
        ending.await.unwrap();

        stopped.store(false, Ordering::Release);
        let (input, owner) = mpsc::channel::<Input>(1);
        drop(owner);
        drain(&input, &stopped).await.expect("no owner left to drain");
    }
}
