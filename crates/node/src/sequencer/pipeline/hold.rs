//! Waiting on the log-sync thread: a request is taken only while the oldest unsynced batch is
//! inside `MAX_SYNC_HOLD`, and every wait on a pending sync while the sequencer runs wakes at that
//! bound to page a hold.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until};
use tracing::{error, info};

use crate::sequencer::pipeline::MAX_IN_FLIGHT;
use crate::sequencer::{Request, Sequencer};

/// How long the oldest batch may wait for its sync before the sequencer stops taking requests and
/// raises Critical `log_sync_held`. An honest disk syncs in milliseconds; a host that holds the sync
/// keeps every command behind it unreleased while it decides whether to let the batch reach the
/// log, so the hold is paged and nothing more is stamped behind it. Measured on the runtime's
/// monotonic clock, whose rate Secure TSC bounds against the host (`clock.rs`).
pub(super) const MAX_SYNC_HOLD: Duration = Duration::from_secs(5);

impl Sequencer {
    /// Releases every batch whose sync has already finished, then judges the oldest one still in
    /// flight against `MAX_SYNC_HOLD`: past it, Critical `log_sync_held` fires once for the episode;
    /// inside it, an open episode ends. Returns the oldest batch's deadline and whether it has
    /// passed, or `None` with nothing in flight.
    /// Error cases:
    /// - a finished sync failed, or its thread ended
    fn check_hold(&mut self) -> anyhow::Result<Option<(Instant, bool)>> {
        while let Some(done) = self.pipeline.as_ref().and_then(|pipeline| pipeline.shared.pop()) {
            self.complete(done)?;
        }
        let oldest = self.pipeline.as_ref().and_then(|pipeline| pipeline.in_flight.front());
        let Some((deadline, through)) = oldest.map(|oldest| (oldest.staged + MAX_SYNC_HOLD, oldest.last_seq)) else {
            self.end_hold();
            return Ok(None);
        };
        let held = Instant::now() >= deadline;
        let pipeline = self.pipeline.as_mut().expect("a batch in flight means a pipeline");
        if !held {
            self.end_hold();
        } else if !std::mem::replace(&mut pipeline.held, true) {
            // The gauge is set first: the page commits to the alert outbox on the same disk, so a
            // whole-disk hold can stall it, while the metrics listener serves from memory.
            self.metrics.log_sync_held.set(1);
            error!(
                alert = "log_sync_held",
                "the log sync of the batch through seq {} has been held for over {MAX_SYNC_HOLD:?}: \
                 sequencing stops until it finishes",
                through.map_or_else(|| "none".to_owned(), |seq| seq.to_string())
            );
        }
        Ok(Some((deadline, held)))
    }

    /// Ends the `log_sync_held` episode, if one is open: no batch in flight is past the bound.
    fn end_hold(&mut self) {
        if self.pipeline.as_mut().is_some_and(|pipeline| std::mem::take(&mut pipeline.held)) {
            self.metrics.log_sync_held.set(0);
            info!(alert_resolved = "log_sync_held", "no log sync is held past the bound: sequencing runs");
        }
    }

    /// Waits for the oldest batch's sync to finish, or for its deadline when that has not passed,
    /// and releases what finished.
    /// Error cases:
    /// - a sync failed, or its thread ended
    async fn await_sync(&mut self, deadline: Instant, held: bool) -> anyhow::Result<()> {
        let shared =
            self.pipeline.as_ref().map(|pipeline| pipeline.shared.clone()).expect("check_hold found a batch in flight");
        tokio::select! {
            biased;
            done = shared.next() => self.complete(done)?,
            () = sleep_until(deadline), if !held => {}
        }
        Ok(())
    }

    /// The next request, releasing batches as their syncs finish meanwhile. With `MAX_IN_FLIGHT`
    /// batches pending, or the oldest one's sync held past `MAX_SYNC_HOLD`, it takes no request
    /// until a sync is done, and it wakes at the deadline with nothing arriving, so a hold pages on
    /// a quiet node too. A request received past the deadline, before the timer fired, is kept and
    /// handed out once the hold clears.
    /// Error cases:
    /// - a sync failed, or its thread ended
    pub(in crate::sequencer) async fn next_request(
        &mut self,
        rx: &mut mpsc::Receiver<Request>,
    ) -> anyhow::Result<Option<Request>> {
        loop {
            let Some((deadline, held)) = self.check_hold()? else {
                let kept = self.pipeline.as_mut().and_then(|pipeline| pipeline.kept.take());
                return Ok(match kept {
                    Some(request) => Some(request),
                    None => rx.recv().await,
                });
            };
            let pipeline = self.pipeline.as_mut().expect("check_hold found a batch in flight");
            if held || pipeline.in_flight.len() >= MAX_IN_FLIGHT {
                self.await_sync(deadline, held).await?;
                continue;
            }
            if let Some(request) = pipeline.kept.take() {
                return Ok(Some(request));
            }
            let shared = pipeline.shared.clone();
            tokio::select! {
                biased;
                done = shared.next() => self.complete(done)?,
                () = sleep_until(deadline) => {}
                request = rx.recv() => match request {
                    Some(request) => {
                        if let Some(request) = self.admit(request, deadline) {
                            return Ok(Some(request));
                        }
                    }
                    None => return Ok(None),
                },
            }
        }
    }

    /// `request`, received while the oldest batch's deadline was `deadline`, if that has not passed;
    /// otherwise it is kept for `next_request` to hand out once the hold clears. A timer can fire
    /// after its deadline, so the receive arm can win a race the bound must not lose.
    pub(super) fn admit(&mut self, request: Request, deadline: Instant) -> Option<Request> {
        if Instant::now() < deadline {
            return Some(request);
        }
        let pipeline = self.pipeline.as_mut().expect("a deadline means a batch in flight");
        pipeline.kept = Some(request);
        None
    }

    /// A request already queued, for the batch being drained, unless the oldest batch's sync is
    /// held past `MAX_SYNC_HOLD`, before or after the receive: the drain then ends, and
    /// `next_request` waits for the sync and hands out a request it kept.
    pub(in crate::sequencer) fn next_queued(&mut self, rx: &mut mpsc::Receiver<Request>) -> Option<Request> {
        let oldest = self.pipeline.as_ref().and_then(|pipeline| pipeline.in_flight.front());
        let deadline = oldest.map(|oldest| oldest.staged + MAX_SYNC_HOLD);
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return None;
        }
        let request = rx.try_recv().ok()?;
        // A receive can wait out a sender finishing its send: past the deadline, the request is kept.
        match deadline {
            Some(deadline) => self.admit(request, deadline),
            None => Some(request),
        }
    }

    /// Waits until every batch in flight is durable and released, paging a hold past the bound.
    /// Error cases:
    /// - a sync failed, or its thread ended
    pub(in crate::sequencer) async fn settle_async(&mut self) -> anyhow::Result<()> {
        while let Some((deadline, held)) = self.check_hold()? {
            self.await_sync(deadline, held).await?;
        }
        Ok(())
    }

    /// `settle_async` for a caller that cannot wait on the runtime. The sequencer's thread is
    /// blocked meanwhile, as an inline sync would. Production reaches it only with the pipeline
    /// drained — setup, before `run` submits a batch, and the end of `run`, after `settle_async` —
    /// so it carries no hold bound.
    /// Error cases:
    /// - as `settle_async`
    pub(in crate::sequencer) fn settle(&mut self) -> anyhow::Result<()> {
        while let Some(pipeline) = self.pipeline.as_ref().filter(|pipeline| !pipeline.in_flight.is_empty()) {
            let shared = pipeline.shared.clone();
            let done = shared.wait();
            self.complete(done)?;
        }
        Ok(())
    }
}
