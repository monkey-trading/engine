//! A following member's provider backfill: fetched while it follows, seeded with its folded marks
//! once its node starts, so its first answer holds the depth.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::{Following, Staged};
use crate::config::{BackfillSection, Venue};
use crate::history::marks::backfill::{Candles, Source};
use crate::history::marks::fold::MarkFold;
use crate::history::marks::run::coverage;
use crate::history::marks::store::Store;
use crate::history::marks::{Resolution, Retentions};
use crate::test_support::{scratch_dir, wait_for};

/// The engine time the tests fetch back from, on a minute boundary.
pub(crate) const NOW: i64 = 1_792_489_800_000;

/// A loopback candles endpoint answering `BTC-USD` with one candle per bucket in the window asked,
/// newest first, counting each request in `hits`; its base URL.
pub(crate) async fn candles_server(hits: Arc<AtomicUsize>) -> String {
    let app = axum::Router::new().route(
        "/products/BTC-USD/candles",
        axum::routing::get(move |query: axum::extract::Query<BTreeMap<String, String>>| {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let seconds = |key: &str| {
                    chrono::DateTime::parse_from_rfc3339(&query[key]).expect("an RFC 3339 window").timestamp()
                };
                let step: i64 = query["granularity"].parse().unwrap();
                let (start, end) = (seconds("start"), seconds("end"));
                let rows: Vec<_> = (0..=(end - start) / step)
                    .rev()
                    .map(|i| serde_json::json!([start + i * step, "67000", "67010", "67005", "67008", "1"]))
                    .collect();
                axum::Json(rows)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// The backfill the tests ask for: ten minutes of 1m candles, and one candle at each other resolution.
pub(crate) fn backfill(base_url: String) -> BackfillSection {
    BackfillSection {
        enabled: true,
        base_url,
        depth_1m: Duration::from_secs(600),
        depth_1h: Duration::ZERO,
        depth_1d: Duration::ZERO,
    }
}

/// The history a member fetched while following reaches its cache with its folded marks: the cache's
/// first answer, once its catch-up has run, holds the whole depth and records the backfill as done,
/// where a member that fetched nothing has an empty cache and a backfill still to run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_following_members_fetch_reaches_its_cache_at_its_first_answer() {
    let hits = Arc::new(AtomicUsize::new(0));
    let cfg = backfill(candles_server(hits.clone()).await);
    let retentions = Retentions::defaults();
    let covers = coverage(&cfg, retentions).unwrap();
    let source = Source { venue: Venue::Coinbase, symbol: "BTC-USD".to_owned() };
    let following = Following::spawn(Candles::new(&cfg.base_url).unwrap(), cfg, BTreeMap::from([(1, source)]), covers);
    following.list(vec![1], NOW);
    // Three resolutions, a request or more each: the market is fetched whole.
    wait_for("the market's fetch", async {
        while hits.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let staged = loop {
        let staged = following.shared.lock().unwrap().staged.done.len();
        if staged == 1 {
            break following.finish().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let mut fold = MarkFold::new(retentions);
    fold.stage(staged);

    let store = Store::open_in(&scratch_dir("follow-first-answer"), retentions, [0x42; 32]).unwrap();
    store.defer();
    let reader = store.reader();
    assert!(!reader.ready());
    let mut store = store;
    store.catch_up(None, Some(fold)).unwrap();
    assert!(reader.ready());
    let page = reader.page(1, Resolution::M1, i64::MIN, i64::MAX, 100, 100).await.unwrap();
    assert_eq!(page.entries.len(), 11, "ten minutes of 1m buckets, both ends");
    assert_eq!(page.entries.first().unwrap().ts, NOW - 600_000);
    assert_eq!(page.entries.last().unwrap().ts, NOW);
    assert_eq!(store.backfilled().unwrap().get(&1), Some(&covers), "the backfill is recorded, so none runs again");

    // A member that fetched nothing has no history and no marker.
    let mut bare = Store::open_in(&scratch_dir("follow-nothing"), retentions, [0x42; 32]).unwrap();
    bare.catch_up(None, Some(MarkFold::new(retentions))).unwrap();
    assert!(bare.backfilled().unwrap().is_empty());
    assert!(bare.reader().page(1, Resolution::M1, i64::MIN, i64::MAX, 100, 100).await.unwrap().entries.is_empty());
}

/// A fetch the member stops part way, by a take or a shutdown, stages no part of a market.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_market_the_member_stopped_part_way_stages_nothing() {
    let hits = Arc::new(AtomicUsize::new(0));
    let cfg = backfill(candles_server(hits.clone()).await);
    let covers = coverage(&cfg, Retentions::defaults()).unwrap();
    let source = Source { venue: Venue::Coinbase, symbol: "BTC-USD".to_owned() };
    let following = Following::spawn(Candles::new(&cfg.base_url).unwrap(), cfg, BTreeMap::from([(1, source)]), covers);
    following.list(vec![1], NOW);
    wait_for("the first request", async {
        while hits.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let Staged { rows, done } = following.finish().await;
    assert!(rows.is_empty() && done.is_empty(), "{} rows staged from a market not finished", rows.len());
}
