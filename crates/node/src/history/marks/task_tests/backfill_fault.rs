//! A cache write that fails during a backfill stops the marks task: the run loop must not file it
//! under the provider failures it retries, because tagged rows of the aborted write may already sit
//! in pages the host can read, and a retry would carry on under the same run key.

use std::collections::BTreeMap;
use std::time::Duration;

use super::{priced_node, prices};
use crate::clock::Clock;
use crate::config::{BackfillSection, Venue};
use crate::history::marks::Retentions;
use crate::history::marks::backfill::{Source, StoreFault};
use crate::history::marks::run;
use crate::history::marks::store::Store;
use crate::test_support::scratch_dir;

#[tokio::test]
async fn a_cache_write_failure_during_a_backfill_stops_the_task() {
    // A candles stand-in with nothing to give: the seed still opens its transaction.
    let app = axum::Router::new()
        .route("/products/BTC-USD/candles", axum::routing::get(|| async move { axum::Json(serde_json::json!([])) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg = BackfillSection {
        enabled: true,
        base_url: format!("http://{addr}"),
        depth_1m: Duration::from_secs(60),
        depth_1h: Duration::ZERO,
        depth_1d: Duration::ZERO,
    };

    // The engine has a time to anchor the backfill on once a command is sequenced.
    let dir = scratch_dir("marks-backfill-fault");
    let (node, clock) = priced_node(&dir);
    node.submit(prices(1, clock.now_ms(), "67000")).await;

    // The host changes the cache's schema: every write now refuses.
    let cache = scratch_dir("marks-backfill-fault-cache");
    let store = Store::open_in(&cache, Retentions::defaults(), [0x42; 32]).unwrap();
    rusqlite::Connection::open(cache.join("marks.sqlite")).unwrap().execute("CREATE TABLE host_note (x)", []).unwrap();

    // No live samples, so the only write is the backfill's.
    let (_samples, rx) = tokio::sync::mpsc::channel(run::QUEUE);
    let live = run::Live { samples: rx, buffered: Vec::new(), flushed_at: tokio::time::Instant::now() };
    let sources = BTreeMap::from([(1, Source { venue: Venue::Coinbase, symbol: "BTC-USD".to_owned() })]);
    let task = tokio::spawn(run::run(store, node.tx.downgrade(), Some(cfg), sources, live, Default::default()));

    let outcome = tokio::time::timeout(Duration::from_secs(60), task).await.expect("the task stops").expect("joined");
    let err = outcome.expect_err("a cache fault ends the task with an error");
    assert!(err.downcast_ref::<StoreFault>().is_some(), "{err:#}");
    node.stop().await;
}
