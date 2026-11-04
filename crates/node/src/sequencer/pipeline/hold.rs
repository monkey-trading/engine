//! Waiting on the log-sync thread: a request is taken only while the oldest unsynced batch is
//! inside `MAX_SYNC_HOLD`, and every wait on a pending sync wakes at that bound to page a hold.

use std::time::Duration;

use anyhow::Context;
use tokio::sync::mpsc;
use tracing::error;

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
    fn check_hold(&mut self) -> anyhow::Result<Option<(tokio::time::Instant, bool)>> {
        while let Some(done) = self.pipeline.as_ref().and_then(|pipeline| pipeline.shared.pop()) {
            self.complete(done)?;
        }
        let Some(pipeline) = self.pipeline.as_mut() else { return Ok(None) };
        let Some(oldest) = pipeline.in_flight.front() else {
            pipeline.end_hold();
            return Ok(None);
        };
        let (deadline, through) = (oldest.staged + MAX_SYNC_HOLD, oldest.last_seq);
        let held = tokio::time::Instant::now() >= deadline;
        if !held {
            pipeline.end_hold();
        } else if !std::mem::replace(&mut pipeline.held, true) {
            error!(
                alert = "log_sync_held",
                "the log sync of the batch through seq {} has been held for over {MAX_SYNC_HOLD:?}: \
                 sequencing stops until it finishes",
                through.map_or_else(|| "none".to_owned(), |seq| seq.to_string())
            );
        }
        Ok(Some((deadline, held)))
    }

    /// Waits for the oldest batch's sync to finish, or for its deadline when that has not passed,
    /// and releases what finished.
    /// Error cases:
    /// - a sync failed, or its thread ended
    async fn await_sync(&mut self, deadline: tokio::time::Instant, held: bool) -> anyhow::Result<()> {
        let shared = self.pipeline.as_ref().map(|pipeline| pipeline.shared.clone()).context("no sync thread")?;
        tokio::select! {
            biased;
            done = shared.next() => self.complete(done)?,
            () = tokio::time::sleep_until(deadline), if !held => {}
        }
        Ok(())
    }

    /// The next request, releasing batches as their syncs finish meanwhile. With `MAX_IN_FLIGHT`
    /// batches pending, or the oldest one's sync held past `MAX_SYNC_HOLD`, it takes no request
    /// until a sync is done, and it wakes at the deadline with nothing arriving, so a hold pages on
    /// a quiet node too.
    /// Error cases:
    /// - a sync failed, or its thread ended
    pub(in crate::sequencer) async fn next_request(
        &mut self,
        rx: &mut mpsc::Receiver<Request>,
    ) -> anyhow::Result<Option<Request>> {
        loop {
            let Some((deadline, held)) = self.check_hold()? else { return Ok(rx.recv().await) };
            let pipeline = self.pipeline.as_ref().expect("a deadline means a batch in flight");
            if held || pipeline.in_flight.len() >= MAX_IN_FLIGHT {
                self.await_sync(deadline, held).await?;
                continue;
            }
            let shared = pipeline.shared.clone();
            tokio::select! {
                biased;
                done = shared.next() => self.complete(done)?,
                () = tokio::time::sleep_until(deadline) => {}
                request = rx.recv() => return Ok(request),
            }
        }
    }

    /// A request already queued, for the batch being drained, unless the oldest batch's sync is
    /// held past `MAX_SYNC_HOLD`: the drain then ends, and `next_request` waits for the sync.
    pub(in crate::sequencer) fn next_queued(&self, rx: &mut mpsc::Receiver<Request>) -> Option<Request> {
        let oldest = self.pipeline.as_ref().and_then(|pipeline| pipeline.in_flight.front());
        if oldest.is_some_and(|oldest| tokio::time::Instant::now() >= oldest.staged + MAX_SYNC_HOLD) {
            return None;
        }
        rx.try_recv().ok()
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
    /// blocked meanwhile, as an inline sync would.
    /// Error cases:
    /// - as `settle_async`
    pub(in crate::sequencer) fn settle(&mut self) -> anyhow::Result<()> {
        while let Some((deadline, held)) = self.check_hold()? {
            let shared = self.pipeline.as_ref().map(|pipeline| pipeline.shared.clone()).context("no sync thread")?;
            let done = if held {
                Some(shared.wait())
            } else {
                shared.wait_for(deadline.saturating_duration_since(tokio::time::Instant::now()))
            };
            if let Some(done) = done {
                self.complete(done)?;
            }
        }
        Ok(())
    }
}
