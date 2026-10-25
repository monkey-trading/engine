//! Test support for the RWA feed tests: schedules open around the clock, signed relistings that
//! move an open or a roll after setup, and a record of every command the sequencer applies.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::DateTime;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::session::{Period, Schedule};
use engine_core::types::{MarketId, Millis};
use k256::ecdsa::SigningKey;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

use crate::admin::sign;
use crate::clock::now_ms;
use crate::feeds::tests::fake::WAIT;
use crate::feeds::tests::scenario::Node;
use crate::sequencer::{Query, Request, StreamBody};
use crate::test_support::dev_config;

/// Every command the sequencer applied, with its stamp and events, in order.
pub type Applied = Arc<Mutex<Vec<(Millis, Command, Vec<Event>)>>>;

impl Node {
    /// Records every command the sequencer applies from now on.
    pub(super) fn record(&self) -> Applied {
        let applied: Applied = Arc::default();
        let (mut stream, sink) = (self.stream.resubscribe(), applied.clone());
        tokio::spawn(async move {
            while let Ok(message) = stream.recv().await {
                if let StreamBody::Command(entry) = &message.body {
                    let events = entry.events.clone().unwrap_or_default();
                    sink.lock().expect("recorder").push((entry.sequenced.ts, entry.sequenced.cmd.clone(), events));
                }
            }
        });
        applied
    }

    /// Relists `spec`, signed by the dev operator at its next nonce, and checks it applied.
    pub(super) async fn relist(&self, spec: MarketSpec) {
        let operator = dev_config().operator().expect("the dev operator");
        let key = SigningKey::from_slice(&[0x42; 32]).expect("the dev operator key");
        let (reply, nonce) = oneshot::channel();
        let query: Query = Box::new(move |engine| {
            let _ = reply.send(engine.next_request_nonce(operator.address));
        });
        self.tx.send(Request::Query(query)).await.expect("sequencer up");
        let nonce = timeout(WAIT, nonce).await.expect("nonce in time").expect("nonce");
        let listing = sign(&key, &operator, AdminCommand::UpsertMarket(spec), nonce, now_ms() + 60_000);
        let events = self.submit(Command::SignedAdmin(Box::new(listing))).await;
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
    }
}

/// The listing of dev genesis market `id`.
pub fn listing(id: MarketId) -> MarketSpec {
    dev_config().genesis.markets.into_iter().find(|market| market.id == id).expect("a dev genesis market")
}

/// Every hour of the week in UTC, less a closure over `[from, to)`, if any.
pub fn around_the_clock(closed: Option<(Millis, Millis)>) -> Schedule {
    let local = |ts: Millis| DateTime::from_timestamp_millis(ts).expect("in range").naive_utc();
    let closures: Vec<Period> =
        closed.map(|(from, to)| Period { from: local(from), to: local(to) }).into_iter().collect();
    let windows = serde_json::json!([{ "open": "Mon 00:00", "close": "Thu 00:00" }, { "open": "Thu 00:00", "close": "Mon 00:00" }]);
    let schedule = serde_json::json!({ "kind": "weekly", "tz": "UTC", "windows": windows, "closures": closures });
    serde_json::from_value(schedule).expect("a weekly schedule")
}

/// Waits until `found` returns something, failing after `within`. `found` reads the record itself,
/// so no lock is held while the sequencer runs.
pub async fn wait_for<T>(within: Duration, found: impl Fn() -> Option<T>) -> T {
    let search = async {
        loop {
            if let Some(found) = found() {
                return found;
            }
            sleep(Duration::from_millis(20)).await;
        }
    };
    timeout(within, search).await.expect("found in time")
}

/// Every command applied so far.
pub fn commands(applied: &Applied) -> Vec<(Millis, Command, Vec<Event>)> {
    applied.lock().expect("recorder").clone()
}

/// Every event applied so far.
pub fn events_of(applied: &Applied) -> Vec<Event> {
    applied.lock().expect("recorder").iter().flat_map(|(_, _, events)| events.clone()).collect()
}

/// When each price command for `market` was stamped, with the contracts its quotes name.
pub fn priced(applied: &Applied, market: MarketId) -> Vec<(Millis, Vec<Option<String>>)> {
    let applied = applied.lock().expect("recorder");
    applied
        .iter()
        .filter_map(|(ts, cmd, _)| match cmd {
            Command::Prices { market: priced, quotes } if *priced == market => {
                Some((*ts, quotes.iter().map(|quote| quote.contract.clone()).collect()))
            }
            _ => None,
        })
        .collect()
}
