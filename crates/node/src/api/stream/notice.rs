//! A signed connection's `notice` frames: the operator's status notice (`GET /v1/notice`) once at the
//! start and again whenever it changes, checked every `api.quote_interval`. It holds no sequencer
//! sender, so a notice subscription never keeps the sequencer from draining at a shutdown.

use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{Interval, MissedTickBehavior, interval};

use crate::api::notice::NoticeView;
use crate::api::{ApiError, ApiState};
use crate::sequencer::Request;

/// A frame's message: the notice, tagged.
#[derive(Serialize)]
#[serde(tag = "type", rename = "notice")]
struct Frame<'a> {
    #[serde(flatten)]
    view: &'a NoticeView,
}

pub(super) struct NoticeFeed {
    /// The API's state with its sequencer sender swapped for a closed one; each read upgrades `tx`.
    state: ApiState,
    tx: mpsc::WeakSender<Request>,
    ticks: Interval,
    /// The read the connection is waiting for, kept across polls: the stream's `select!` drops this
    /// future whenever a log message or heartbeat wins, and the read must outlive that.
    reading: Option<BoxFuture<'static, Result<NoticeView, ApiError>>>,
    /// The last notice sent; `None` before the first.
    sent: Option<NoticeView>,
}

impl NoticeFeed {
    pub(super) fn new(state: &ApiState, every: Duration) -> Self {
        let tx = state.tx.downgrade();
        let mut state = state.clone();
        state.tx = mpsc::channel(1).0;
        // Nor the live log's sender, which would keep the stream from seeing the log's feed end.
        state.stream = broadcast::channel(1).0;
        let mut ticks = interval(every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self { state, tx, ticks, reading: None, sent: None }
    }

    /// The next notice message: the first at once, then each time the notice differs from the last
    /// sent. Waits while it is unchanged or cannot be read.
    pub(super) async fn next(&mut self) -> Vec<String> {
        loop {
            if self.reading.is_none() {
                self.ticks.tick().await;
                // A stopped sequencer has no state to serve; the stream ends with the log's feed.
                let Some(tx) = self.tx.upgrade() else { return std::future::pending().await };
                let mut state = self.state.clone();
                state.tx = tx;
                self.reading = Some(Box::pin(async move { state.current_notice().await }));
            }
            let read = self.reading.as_mut().expect("a read was just started").await;
            self.reading = None;
            let Ok(view) = read else { continue };
            if self.sent.as_ref() == Some(&view) {
                continue;
            }
            let text = serde_json::to_string(&Frame { view: &view }).expect("a notice frame encodes");
            self.sent = Some(view);
            return vec![text];
        }
    }
}
