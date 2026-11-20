//! A feed that holds its quotes back until the node that sequences them runs. A rotation-set member
//! opens its venue connections a lead before its own turn's boundary (`node::price_feeds::Warm`), so
//! they are subscribed, their books current and their pings answered by the time it sequences its first
//! command. While it holds back, a feed sends the sequencer nothing: it keeps each symbol's newest
//! quote, with the delay bound noted when it was read or re-dated, asks the engine nothing, and counts
//! no symbol as silent. It judges its markets' hours by those its member's copy of the log holds, so a
//! venue that may go quiet through a closure does. The first look after it goes live, before anything
//! is sent, restarts its counts and asks the engine about its symbols, as a fresh connection does; it
//! then sends the quotes it kept, which the sequencer holds to the same bounds as any other: the delay
//! bound its pings set, checked at the stamp, and the market's `max_age_ms`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use engine_core::session::Schedule;
use engine_core::types::MarketId;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::feeds::venue::{Pending, VenueFeed};

/// When a feed's quotes go to the sequencer, and the markets' hours it starts from.
#[derive(Clone, Debug)]
pub struct Held {
    /// Set once the node that sequences the quotes runs.
    pub live: watch::Receiver<bool>,
    /// Each market's trading hours, as the engine last held them; empty for none known.
    pub hours: Arc<BTreeMap<MarketId, Schedule>>,
}

impl Held {
    /// A feed its node starts: live from the outset, with no hours known until the engine answers.
    pub fn never() -> Self {
        Self { live: watch::channel(true).1, hours: Arc::default() }
    }
}

impl VenueFeed {
    /// This feed, holding its quotes back as `held` says, with its markets' hours. One held back from the
    /// outset is holding before any connection opens, so its going live restarts its counts however its
    /// attempts went meanwhile.
    pub(super) fn held_back_by(self, held: Held) -> Self {
        let holding = !*held.live.borrow();
        Self { live: held.live, schedules: held.hours, holding, ..self }
    }

    /// Whether the switch lets the feed's quotes through.
    pub(super) fn live(&self) -> bool {
        *self.live.borrow()
    }

    /// Whether the feed holds its quotes back now. The first look after its switch is set, before
    /// anything is sent, restarts every symbol's silence and the connection's counts, as a fresh
    /// connection does, and asks the engine about its symbols: nothing it held back counts against it,
    /// and no quote goes ahead of the question.
    /// Error cases:
    /// - the sequencer has gone away
    pub(super) async fn holds_back(&mut self) -> anyhow::Result<bool> {
        if !self.live() {
            self.holding = true;
            return Ok(true);
        }
        if std::mem::take(&mut self.holding) {
            let now = Instant::now();
            self.last_quote.values_mut().for_each(|at| *at = now);
            self.connected_afresh();
            if self.confirming.is_none() {
                self.confirming = Some(self.confirm().await?);
            }
        }
        Ok(false)
    }

    /// Waits `wait` before a held-back feed connects again, or less once its switch is set: a feed going
    /// live does not sit out a backoff it built up while held back. Whether the switch cut it short.
    pub(super) async fn wait_unless_live(&self, wait: Duration) -> bool {
        let mut live = self.live.clone();
        // A switch whose node has gone sets nothing: the wait runs out, and the task is stopped.
        let set = async {
            if live.wait_for(|live| *live).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => false,
            () = set => true,
        }
    }

    /// Whether the feed still has to take its switch's being set (see `holds_back`).
    pub(super) fn holding(&self) -> bool {
        self.holding
    }

    /// Keeps only what a feed holding back sends once live: each symbol's newest quote and its delay
    /// bound. The bounds noted for the quotes it replaced are forgotten, since nothing takes them, and
    /// the kept quote counts its wait to the sequencer from when it is sent, as a repeat held to refresh
    /// its mid does.
    pub(super) fn keep_newest(&mut self, buffered: &mut BTreeMap<String, Pending>) {
        for (symbol, quotes) in buffered.iter_mut() {
            if let Some(ts) = quotes.kept_ts() {
                self.delay.forget_before(symbol, ts);
            }
            quotes.take_received();
        }
    }
}
