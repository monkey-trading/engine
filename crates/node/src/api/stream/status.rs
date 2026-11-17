//! A signed connection's `status` frames: the operating state (`GET /v1/status`) once at the start
//! and again whenever it changes, checked every `api.quote_interval`. Every connection shares one read
//! an interval (`ApiState::shared_operating_state`), and holds no sequencer sender, so a status
//! subscription never keeps the sequencer from draining at a shutdown.

use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{Interval, MissedTickBehavior, interval};

use crate::api::{ApiError, ApiState};
use crate::sequencer::Request;
use crate::status::StatusView;

/// A frame's message: the state, tagged.
#[derive(Serialize)]
#[serde(tag = "type", rename = "status")]
struct Frame<'a> {
    #[serde(flatten)]
    view: &'a StatusView,
}

pub(super) struct StatusFeed {
    /// The API's state with its sequencer sender swapped for a closed one; each read upgrades `tx`.
    state: ApiState,
    tx: mpsc::WeakSender<Request>,
    every: Duration,
    ticks: Interval,
    /// The read the connection is waiting for, kept across polls: the stream's `select!` drops this
    /// future whenever a log message or heartbeat wins, and the read must outlive that.
    reading: Option<BoxFuture<'static, Result<StatusView, ApiError>>>,
    /// The last state sent, without its read time.
    sent: Option<String>,
}

impl StatusFeed {
    pub(super) fn new(state: &ApiState, every: Duration) -> Self {
        let tx = state.tx.downgrade();
        let mut state = state.clone();
        state.tx = mpsc::channel(1).0;
        // Nor the live log's sender, which would keep the stream from seeing the log's feed end.
        state.stream = broadcast::channel(1).0;
        let mut ticks = interval(every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self { state, tx, every, ticks, reading: None, sent: None }
    }

    /// The next status message: the first at once, then each time the state differs from the last
    /// sent. Waits while the state is unchanged or cannot be read.
    pub(super) async fn next(&mut self) -> Vec<String> {
        loop {
            if self.reading.is_none() {
                self.ticks.tick().await;
                // A stopped sequencer has no state to serve; the stream ends with the log's feed.
                let Some(tx) = self.tx.upgrade() else { return std::future::pending().await };
                let (mut state, every) = (self.state.clone(), self.every);
                state.tx = tx;
                self.reading = Some(Box::pin(async move { state.shared_operating_state(every).await }));
            }
            let read = self.reading.as_mut().expect("a read was just started").await;
            self.reading = None;
            let Ok(mut view) = read else { continue };
            view.as_of_ms = 0;
            let key = serde_json::to_string(&view).expect("a status encodes");
            if self.sent.as_deref() == Some(key.as_str()) {
                continue;
            }
            view.as_of_ms = crate::clock::now_ms();
            let text = serde_json::to_string(&Frame { view: &view }).expect("a status frame encodes");
            self.sent = Some(key);
            return vec![text];
        }
    }
}
