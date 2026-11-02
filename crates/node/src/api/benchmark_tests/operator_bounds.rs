//! The operator's bounds on a dev node, through the real publisher worker, signed admin API, TLS and
//! log, on deterministic time: a rate outside the cap is refused while one inside applies, the
//! worker leaves out what the cap refuses and signs the rest, a stress floor above level 1 is
//! refused while one at level 1 lapses after 72 hours, and a restart and a public replay reach the
//! same state.

use super::*;

use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
use engine_core::protocol::ProtocolVersion;
use engine_core::stress::STRESS_FLOOR_LAPSE_MS;
use k256::ecdsa::SigningKey;

use crate::admin::NodeApi;
use crate::test_support::capture_alerts;

/// Serves SOFR and ESTR histories to 19 October, and once `published` is set their next rates, for
/// 21 October: an outlying SOFR and an ordinary ESTR.
async fn publishers() -> (String, Arc<AtomicUsize>, JoinHandle<std::io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let published = Arc::new(AtomicUsize::new(0));
    let (sofr, estr) = (published.clone(), published.clone());
    let app = Router::new()
        .route(
            "/sofr",
            get(move || {
                let later = sofr.load(Ordering::SeqCst) > 0;
                async move {
                    if later {
                        "Effective Date,Rate Type,Rate (%)\n10/19/2026,SOFR,4.00\n10/21/2026,SOFR,7.50\n"
                    } else {
                        "Effective Date,Rate Type,Rate (%)\n10/19/2026,SOFR,4.00\n"
                    }
                }
            }),
        )
        .route(
            "/estr",
            get(move || {
                let later = estr.load(Ordering::SeqCst) > 0;
                async move {
                    if later {
                        "KEY,TIME_PERIOD,OBS_VALUE\nEST.B.EU000A2X2A25.WT,2026-10-19,2.00\nEST.B.EU000A2X2A25.WT,2026-10-21,2.01\n"
                    } else {
                        "KEY,TIME_PERIOD,OBS_VALUE\nEST.B.EU000A2X2A25.WT,2026-10-19,2.00\n"
                    }
                }
            }),
        );
    (url, published, tokio::spawn(async move { axum::serve(listener, app).await }))
}

/// Signs `command` as the dev operator with its next nonce, and returns the events it sequenced.
async fn signed(node: &Node, command: AdminCommand) -> Vec<Event> {
    let operator = dev_config().operator().unwrap();
    let nonce = query(node, move |engine| engine.next_request_nonce(operator.address)).await;
    let key = SigningKey::from_slice(&[0x42; 32]).unwrap();
    let request = admin::sign(&key, &operator, command, nonce, node.clock.now_ms() + 150_000);
    let api = NodeApi { url: node.url.parse().unwrap(), pin: hex::decode(&node.pin).unwrap().try_into().unwrap() };
    admin::submit(&api, &request).await.unwrap()
}

async fn query<T: Send + 'static>(node: &Node, read: impl FnOnce(&Engine) -> T + Send + 'static) -> T {
    let (reply, seen) = oneshot::channel();
    let query = Box::new(move |engine: &Engine| {
        let _ = reply.send(read(engine));
    });
    node.tx.send(SeqRequest::Query(query)).await.unwrap();
    seen.await.unwrap()
}

/// The rate history holds for `benchmark` on `date`.
async fn observed(node: &Node, benchmark: &'static str, date: chrono::NaiveDate) -> Option<Fixed> {
    query(node, move |engine| engine.benchmark_history().get(benchmark).and_then(|h| h.get(&date)).copied()).await
}

/// A snapshot that signs one SOFR rate for `date`.
fn sofr_snapshot(date: chrono::NaiveDate, rate: &str) -> AdminCommand {
    AdminCommand::SetBenchmarkRates(BenchmarkBatch {
        as_of: date,
        checked: ["SOFR".to_owned(), "ESTR".to_owned()].into(),
        rates: vec![BenchmarkRate { benchmark: "SOFR".into(), effective_date: date, annual_rate: fixed(rate) }],
    })
}

/// BTC's effective stress level, and its floor.
async fn stress(node: &Node) -> (usize, usize) {
    query(node, |engine| {
        let market = &engine.markets()[&1];
        (market.stress.level, market.stress.floor)
    })
    .await
}

/// A node under the latest protocol at `at`, whose API reads `clock`.
async fn start(dir: &Path, at: Millis, clock: Arc<TestClock>) -> Node {
    Node::launch(dir, at, clock, std::convert::identity, ProtocolVersion::LATEST).await
}

/// Moves the sequenced time and the API's clock to `to`.
async fn advance(node: &Node, clock: &TestClock, to: Millis) {
    clock.advance(Duration::from_millis((to - clock.now_ms()) as u64));
    node.advance(to).await;
}

#[tokio::test]
async fn dev_scenario_the_operator_is_bounded_on_benchmark_rates_and_the_stress_floor() {
    let (_alerts_guard, mut alerts) = capture_alerts();
    let dir = scratch_dir("operator-bounds-scenario");
    fs::create_dir_all(&dir).unwrap();
    let (publisher, published, publishers) = publishers().await;
    let cfg_path = dir.join("node.toml");
    let mut text = fs::read_to_string(crate::test_support::DEV_CONFIG).unwrap();
    text.push_str(&format!(
        r#"
[benchmarks]
interval = "1h"
timeout = "2s"
[[benchmarks.sources]]
publisher = "sofr"
endpoint = "{publisher}/sofr"
[[benchmarks.sources]]
publisher = "estr"
endpoint = "{publisher}/estr"
"#
    ));
    fs::write(&cfg_path, text).unwrap();
    let tuesday: chrono::NaiveDate = "2026-10-20".parse().unwrap();
    let wednesday: chrono::NaiveDate = "2026-10-21".parse().unwrap();
    let clock = Arc::new(TestClock::at(START));

    // 1. The worker's first snapshot releases the hold on a fresh node. The dev genesis dates its own
    //    rates today, as a benchmark's first rates, and the publishers' earlier rates are within the
    //    cap of them.
    let node = start(&dir, START, clock.clone()).await;
    assert!(node.try_submit(Command::Tick).await.is_none(), "held for the first snapshot");
    node.sync(&cfg_path, || START).await;
    assert_eq!(observed(&node, "SOFR", tuesday).await, Some(fixed("0.04")));

    // 2. A day later the publisher's SOFR is 350 bps from the last applied. The worker leaves it out,
    //    raises an alert, and signs ESTR's, which applies.
    let wednesday_noon = START + 86_400_000;
    advance(&node, &clock, wednesday_noon).await;
    published.store(1, Ordering::SeqCst);
    let request = node.sync(&cfg_path, || START + 86_400_000).await;
    let AdminCommand::SetBenchmarkRates(batch) = &request.command else { panic!("a snapshot") };
    assert!(batch.rates.iter().all(|rate| rate.benchmark != "SOFR" || rate.effective_date != wednesday), "{batch:?}");
    assert_eq!(observed(&node, "SOFR", wednesday).await, None);
    assert_eq!(observed(&node, "ESTR", wednesday).await, Some(fixed("0.0201")));
    let mut raised = Vec::new();
    while let Ok(alert) = alerts.try_recv() {
        raised.push(alert.name);
    }
    assert!(raised.iter().any(|name| name == "benchmark_rate_refused"), "{raised:?}");

    // 3. The operator signing that rate itself is refused, and a rate inside the cap applies.
    let events = signed(&node, sofr_snapshot(wednesday, "0.075")).await;
    assert!(
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("moves more than 300 bps a day")),
        "{events:?}"
    );
    assert_eq!(observed(&node, "SOFR", wednesday).await, None);
    let events = signed(&node, sofr_snapshot(wednesday, "0.069")).await;
    assert_eq!(events.last(), Some(&Event::AdminApplied));
    assert_eq!(observed(&node, "SOFR", wednesday).await, Some(fixed("0.069")));

    // 4. A floor at level 2 is refused, for a market and for all of them; one at level 1 applies.
    let floor = |market, level| AdminCommand::SetStressFloor { market, level };
    let top = dev_config().genesis.params.stress_levels.len() - 1;
    assert!(top >= 2, "the dev config has a level above the operator's");
    for market in [Some(1), None] {
        let events = signed(&node, floor(market, 2)).await;
        assert_eq!(
            events.last(),
            Some(&Event::AdminRejected { reason: "the operator's stress floor is at most level 1".into() })
        );
    }
    assert_eq!(stress(&node).await, (0, 0));
    assert_eq!(signed(&node, floor(Some(1), 1)).await.last(), Some(&Event::AdminApplied));
    assert_eq!(stress(&node).await, (1, 1));
    let set = node.clock.now_ms();

    // 5. A millisecond before its 72 hours the floor still holds, across a restart, and the restarted
    //    worker's snapshot still leaves out what the cap refuses.
    let held = set + STRESS_FLOOR_LAPSE_MS - 1;
    advance(&node, &clock, held).await;
    node.submit(Command::Tick).await;
    assert_eq!(stress(&node).await, (1, 1));
    node.stop().await;
    let node = start(&dir, held, clock.clone()).await;
    node.sync(&cfg_path, move || held).await;
    assert_eq!(stress(&node).await, (1, 1), "the floor's time survives a restart");
    assert_eq!(observed(&node, "SOFR", wednesday).await, Some(fixed("0.069")));

    // 6. At its 72 hours it lapses to the automatic level, on the sequencer's stamp.
    let lapsed = set + STRESS_FLOOR_LAPSE_MS;
    advance(&node, &clock, lapsed).await;
    let events = node.submit(Command::Tick).await;
    assert!(events.contains(&Event::StressChanged { market: 1, level: 0 }), "{events:?}");
    assert_eq!(stress(&node).await, (0, 0));
    let history = query(&node, |engine| engine.benchmark_history().clone()).await;
    let market = query(&node, |engine| serde_json::to_value(&engine.markets()[&1].stress).unwrap()).await;
    node.stop().await;
    publishers.abort();

    // 7. A restart and a public replay reach the same state.
    let node = start(&dir, lapsed, clock.clone()).await;
    assert_eq!(query(&node, |engine| engine.benchmark_history().clone()).await, history);
    node.stop().await;
    let replayed = replay(&log_entries(&dir)).unwrap();
    assert_eq!(*replayed.engine.benchmark_history(), history);
    assert_eq!(serde_json::to_value(&replayed.engine.markets()[&1].stress).unwrap(), market);
}
