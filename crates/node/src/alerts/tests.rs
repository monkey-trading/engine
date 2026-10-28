use super::*;

use std::sync::Mutex;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use prometheus_client::metrics::counter::Counter;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tracing::field::Empty;
use tracing::subscriber::{set_default, with_default};
use tracing::{error, info, info_span, warn};
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{fmt, registry};

use crate::alerts::webhook::Retry;
use crate::config::{AlertsSection, OutboxSection, WebhookRoute, WebhookUrl};
use crate::metrics::SeverityLabel;
use crate::test_support::Written;

/// Retries in milliseconds, giving up only after a minute, far longer than any test runs.
const FAST: Retry =
    Retry { first: Duration::from_millis(1), max: Duration::from_millis(4), give_up_after: Duration::from_secs(60) };
/// Retries in milliseconds, giving up after a twentieth of a second.
const GIVE_UP_SOON: Retry =
    Retry { first: Duration::from_millis(1), max: Duration::from_millis(4), give_up_after: Duration::from_millis(50) };

#[test]
fn an_error_alert_is_a_critical_line_with_its_fields_and_span() {
    let lines = capture(|| {
        let _span = info_span!("command", id = 42).entered();
        error!(alert = "deposit_rejected", amount = 5, account = "0xaa", "deposit not credited");
    });
    let [line] = lines.as_slice() else { panic!("{lines:?}") };
    assert_eq!(line.name, "deposit_rejected");
    assert_eq!(line.severity, Some(Severity::Critical));
    assert_eq!(line.fields, [("account".to_owned(), "0xaa".to_owned()), ("amount".to_owned(), "5".to_owned())]);
    assert_eq!(line.summary, "deposit not credited");
    assert_eq!(line.context, "command{id=42}");
    assert_eq!(line.key(), "deposit_rejected{account=0xaa,amount=5}");
}

#[test]
fn a_span_field_recorded_after_creation_reaches_the_context() {
    let lines = capture(|| {
        let span = info_span!("http", method = "POST", id = Empty).entered();
        span.record("id", "0xaa");
        error!(alert = "custody_unavailable", "no consumer");
    });
    let [line] = lines.as_slice() else { panic!("{lines:?}") };
    assert_eq!(line.context, "http{method=POST id=0xaa}");
}

#[test]
fn warn_alerts_are_warnings_and_info_resolve_lines_are_captured() {
    let lines = capture(|| {
        warn!(alert = "prices_rejected", market = 1, "price update refused");
        info!(alert_resolved = "market_no_price", market = 1, "priced again");
        // Neither is an alert line: `alert` below `warn`, and an error without `alert`.
        info!(alert = "not_an_alert", "ignored");
        error!(market = 1, "plain error");
    });
    let [warning, resolve] = lines.as_slice() else { panic!("{lines:?}") };
    assert_eq!((warning.name.as_str(), warning.severity), ("prices_rejected", Some(Severity::Warning)));
    assert_eq!((resolve.name.as_str(), resolve.severity), ("market_no_price", None));
    assert_eq!(resolve.key(), "market_no_price{market=1}");
}

#[test]
fn a_full_queue_drops_and_counts_lines_without_waiting() {
    let metrics = Arc::new(Metrics::new());
    let (tx, mut rx) = mpsc::channel(1);
    let layer = AlertLayer { lines: tx, metrics: metrics.clone() };
    let subscriber = registry().with(layer.with_filter(filter_fn(is_alert_callsite)));
    with_default(subscriber, || {
        for _ in 0..3 {
            error!(alert = "flood", "again");
        }
    });
    assert_eq!(rx.try_recv().unwrap().name, "flood");
    assert!(rx.try_recv().is_err());
    assert_eq!(metrics.alerts_dropped.get(), 2);
}

#[test]
fn an_episode_pages_once_until_it_has_been_quiet() {
    let mut episodes = Episodes::new(Duration::from_secs(10));
    let t0 = Instant::now();
    let at = |secs| t0 + Duration::from_secs(secs);
    assert_eq!(status(episodes.on_line(alert("flood", 1), at(0))), Some(Status::Firing));
    // Each repeat restarts the quiet period, so a steady flood never pages twice.
    assert_eq!(episodes.on_line(alert("flood", 1), at(5)), None);
    assert_eq!(episodes.on_line(alert("flood", 1), at(14)), None);
    assert_eq!(status(episodes.on_line(alert("flood", 1), at(24))), Some(Status::Firing));
    // Another key is its own episode.
    assert_eq!(status(episodes.on_line(alert("flood", 2), at(24))), Some(Status::Firing));
}

#[test]
fn a_resolve_ends_its_episode_on_the_alerts_route_even_after_the_quiet_period() {
    let mut episodes = Episodes::new(Duration::from_secs(10));
    let t0 = Instant::now();
    episodes.on_line(warning("market_no_price", 1), t0).unwrap();
    let Some(page) = episodes.on_line(resolve("market_no_price", 1), t0 + Duration::from_secs(3600)) else {
        panic!("expected a page for the warning route");
    };
    assert_eq!((page.status, page.severity), (Status::Resolved, Severity::Warning));
    assert_eq!(page.key, "market_no_price{market=1}");
    // Ended: the next alert pages again.
    assert_eq!(status(episodes.on_line(alert("market_no_price", 1), t0)), Some(Status::Firing));
}

#[tokio::test]
async fn routing_posts_one_page_per_episode_start_and_end_to_its_severitys_webhook() {
    let receiver = Receiver::start(vec![]).await;
    let metrics = Arc::new(Metrics::new());
    let routes = routes(Some(&receiver.url("/hook/secret-token")), None);
    let (layer, router) = channel(routes, metrics.clone());
    let routing = router.spawn(Duration::from_secs(3600), &outbox_config()).await.unwrap();
    for line in [alert("market_no_price", 1), alert("market_no_price", 1), warning("bad_debt", 1)] {
        layer.lines.send(line).await.unwrap();
    }
    layer.lines.send(resolve("market_no_price", 1)).await.unwrap();
    // Drain delivers what was logged before it.
    routing.drain(Duration::from_secs(10)).await;

    let bodies = receiver.bodies();
    let expected = |status| {
        json!({
            "status": status, "severity": "critical", "alert": "market_no_price",
            "key": "market_no_price{market=1}", "summary": "summary", "fields": { "market": "1" },
            "context": "command{id=7}", "ts_ms": 1_700_000_000_000_i64,
        })
    };
    assert_eq!(bodies, [expected("firing"), expected("resolved")]);
    assert_eq!(delivered(&metrics, "critical", "sent"), 2);
    // The warning has no route, so it takes no queue slot.
    assert_eq!(delivered(&metrics, "warning", "dropped"), 0);
}

#[tokio::test]
async fn a_full_outbox_refuses_the_page_counts_overflow_and_forgets_its_episode() {
    let metrics = Arc::new(Metrics::new());
    let mut episodes = Episodes::new(Duration::from_secs(3600));
    let t0 = Instant::now();
    let (queue, _queued) = mpsc::channel(1);
    let mut tasks = JoinSet::new();
    let cfg = OutboxSection { max_pages: 1, ..outbox_config() };
    let outbox = Outbox::start(cfg, [true, false], metrics.clone(), &mut tasks).await.unwrap();
    let queues = Queues { critical: Some(queue), warning: None, outbox };
    for market in 0..3 {
        let Some(page) = episodes.on_line(alert("flood", market), t0) else { panic!() };
        enqueue(&queues, &mut episodes, page, &metrics).await.unwrap();
    }
    assert_eq!(queues.outbox.next(Severity::Critical).await.unwrap().unwrap().page.key, "flood{market=0}");
    assert_eq!(delivered(&metrics, "critical", "dropped"), 2);
    assert_eq!(metrics.alert_outbox_overflows.get(), 2);
    assert_eq!(pending(&metrics, "critical"), 1);
    assert!(episodes.on_line(alert("flood", 1), t0).is_some());
    assert_eq!(episodes.on_line(alert("flood", 0), t0), None);
    drop(queues);
    while tasks.join_next().await.is_some() {}
}

/// Every way a post can end, through the route queue and its delivery task: the page is counted
/// once as sent or failed, every failed attempt is counted as it happens, and nothing is left
/// pending.
#[tokio::test]
async fn every_delivery_outcome_is_counted_and_leaves_nothing_pending() {
    use StatusCode as S;
    // (answers before a 200, posts the webhook sees, sent, failed, failed attempts)
    let rows: [(Vec<StatusCode>, usize, u64, u64, u64); 6] = [
        (vec![], 1, 1, 0, 0),
        (vec![S::INTERNAL_SERVER_ERROR, S::REQUEST_TIMEOUT, S::TOO_MANY_REQUESTS, S::BAD_GATEWAY], 5, 1, 0, 4),
        (vec![S::TEMPORARY_REDIRECT], 1, 0, 1, 1),
        (vec![S::BAD_REQUEST], 1, 0, 1, 1),
        (vec![S::UNAUTHORIZED], 1, 0, 1, 1),
        (vec![S::NOT_FOUND], 1, 0, 1, 1),
    ];
    for (answers, posts, sent, failed, attempts) in rows {
        let row = format!("{answers:?}");
        let receiver = Receiver::start(answers).await;
        let metrics = Arc::new(Metrics::new());
        deliver_through_route(&receiver.url("/hook"), &metrics, FAST).await;
        assert_eq!(receiver.requests(), posts, "{row}");
        assert_eq!(receiver.bodies().len() as u64, sent, "{row}");
        let counted = (delivered(&metrics, "critical", "sent"), delivered(&metrics, "critical", "failed"));
        assert_eq!(counted, (sent, failed), "{row}");
        assert_eq!(attempts_failed(&metrics, "critical"), attempts, "{row}");
        assert_eq!(pending(&metrics, "critical"), 0, "{row}");
    }
}

#[tokio::test]
async fn an_unreachable_webhook_is_given_up_counted_and_its_error_omits_the_url() {
    // Bound, then closed: nothing listens on the port.
    let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}/hook/secret-token");
    let err =
        webhook::post(&client().unwrap(), &secret(&url), &page(), GIVE_UP_SOON, &Counter::default()).await.unwrap_err();
    assert!(format!("{err:#}").contains("gave up after"), "{err:#}");
    assert!(!format!("{err:#}").contains("secret-token"), "{err:#}");

    let metrics = Arc::new(Metrics::new());
    deliver_through_route(&url, &metrics, GIVE_UP_SOON).await;
    assert_eq!(delivered(&metrics, "critical", "failed"), 1);
    assert!(attempts_failed(&metrics, "critical") >= 1);
    assert_eq!(pending(&metrics, "critical"), 0);
}

/// Whichever routing task dies, `stopped` reports it, which stops the node, and `drain` still
/// returns.
#[tokio::test]
async fn a_routing_task_that_dies_is_reported_and_drain_still_returns() {
    for (dying, reported) in [("router", "alert router task panicked"), ("delivery", "alert delivery task panicked")] {
        let (drain_tx, drain_rx) = oneshot::channel::<()>();
        let survivor = async move {
            let _ = drain_rx.await;
        };
        let mut deliveries = JoinSet::new();
        let router = if dying == "router" {
            deliveries.spawn(survivor);
            tokio::spawn(async { panic!("router bug") })
        } else {
            deliveries.spawn(async { panic!("delivery bug") });
            tokio::spawn(survivor)
        };
        let mut routing = Routing { router, deliveries, drain: Some(drain_tx), metrics: Arc::new(Metrics::new()) };
        let err = timeout(Duration::from_secs(5), routing.stopped()).await.expect("the dead task is reported");
        assert!(format!("{err:#}").contains(reported), "{dying}: {err:#}");
        // Returns, rather than polling a dead task's handle a second time.
        routing.drain(Duration::from_secs(5)).await;
    }
}

#[tokio::test]
async fn a_stopped_route_leaves_its_committed_page_pending_and_stops_the_router() {
    let metrics = Arc::new(Metrics::new());
    let mut tasks = JoinSet::new();
    let outbox = Outbox::start(outbox_config(), [true, false], metrics.clone(), &mut tasks).await.unwrap();
    let mut episodes = Episodes::new(Duration::from_secs(3600));
    let (queue, delivery) = mpsc::channel(1);
    drop(delivery);
    let queues = Queues { critical: Some(queue), warning: None, outbox };
    let Some(page) = episodes.on_line(alert("flood", 1), Instant::now()) else { panic!() };
    assert!(enqueue(&queues, &mut episodes, page, &metrics).await.unwrap_err().to_string().contains("stopped"));
    assert_eq!(pending(&metrics, "critical"), 1);
    assert_eq!(queues.outbox.next(Severity::Critical).await.unwrap().unwrap().page.key, "flood{market=1}");
    drop(queues);
    while tasks.join_next().await.is_some() {}
}

#[tokio::test]
async fn drain_gives_up_after_its_grace_and_stops_the_tasks() {
    let (router_alive, router_stopped) = oneshot::channel::<()>();
    let (delivery_alive, delivery_stopped) = oneshot::channel::<()>();
    let mut deliveries = JoinSet::new();
    deliveries.spawn(async move {
        let _alive = delivery_alive;
        std::future::pending::<()>().await;
    });
    let router = tokio::spawn(async move {
        let _alive = router_alive;
        std::future::pending::<()>().await;
    });
    let metrics = Arc::new(Metrics::new());
    metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity: "critical" }).set(2);
    let routing = Routing { router, deliveries, drain: None, metrics };
    let written = Written::default();
    let _logging = set_default(registry().with(fmt::layer().with_writer(written.clone()).with_ansi(false)));
    routing.drain(Duration::from_millis(10)).await;
    // The retained pending count is logged.
    let text = written.text();
    assert!(text.contains("committed pages remain in the outbox") && text.contains("critical=2 warning=0"), "{text}");
    // Each task's sender is dropped only when the task is aborted.
    let stopped = |rx: oneshot::Receiver<()>| timeout(Duration::from_secs(5), rx);
    assert!(matches!(stopped(router_stopped).await, Ok(Err(_))));
    assert!(matches!(stopped(delivery_stopped).await, Ok(Err(_))));
}

// ── Helpers ──

fn alert(name: &str, market: u64) -> AlertLine {
    AlertLine {
        name: name.to_owned(),
        severity: Some(Severity::Critical),
        fields: vec![("market".to_owned(), market.to_string())],
        summary: "summary".to_owned(),
        context: "command{id=7}".to_owned(),
        ts_ms: 1_700_000_000_000,
    }
}

fn warning(name: &str, market: u64) -> AlertLine {
    AlertLine { severity: Some(Severity::Warning), ..alert(name, market) }
}

fn resolve(name: &str, market: u64) -> AlertLine {
    AlertLine { severity: None, ..alert(name, market) }
}

fn page() -> Page {
    let line = alert("market_no_price", 1);
    let key = line.key();
    Page::new(line, key, Status::Firing, Severity::Critical)
}

fn status(routed: Option<Page>) -> Option<Status> {
    Some(routed?.status)
}

/// Routes to `critical` and `warning`, through environment variables.
fn routes(critical: Option<&str>, warning: Option<&str>) -> Routes {
    let cfg = AlertsSection {
        outbox: outbox_config(),
        no_price_after: Duration::from_secs(5),
        episode_quiet: Duration::from_secs(60),
        critical: critical.map(|_| WebhookRoute { url_env: "CRITICAL".into() }),
        warning: warning.map(|_| WebhookRoute { url_env: "WARNING".into() }),
    };
    let env = |var: &str| match var {
        "CRITICAL" => critical.map(str::to_owned),
        "WARNING" => warning.map(str::to_owned),
        _ => None,
    };
    Routes::from_config(&cfg, env).unwrap()
}

fn secret(url: &str) -> WebhookUrl {
    WebhookUrl::from_env("HOOK", |_| Some(url.to_owned())).unwrap()
}

/// Delivers one Critical page to `url` the way routing does: queued on the route, then posted by its
/// delivery task.
async fn deliver_through_route(url: &str, metrics: &Arc<Metrics>, retry: Retry) {
    let mut tasks = JoinSet::new();
    let outbox = Outbox::start(outbox_config(), [true, false], metrics.clone(), &mut tasks).await.unwrap();
    let queues = Queues::start(routes(Some(url), None), client().unwrap(), metrics, retry, outbox, &mut tasks);
    let mut episodes = Episodes::new(Duration::from_secs(60));
    enqueue(&queues, &mut episodes, page(), metrics).await.unwrap();
    drop(queues);
    while tasks.join_next().await.is_some() {}
}

fn pending(metrics: &Metrics, severity: &'static str) -> i64 {
    metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity }).get()
}

fn attempts_failed(metrics: &Metrics, severity: &'static str) -> u64 {
    metrics.alert_attempts_failed.get_or_create(&SeverityLabel { severity }).get()
}

fn delivered(metrics: &Metrics, severity: &'static str, outcome: &'static str) -> u64 {
    metrics.alert_deliveries.get_or_create(&DeliveryLabels { severity, outcome }).get()
}

/// A local webhook receiver: it answers with `failures` in order, then 200, and keeps the bodies
/// it accepted.
struct Receiver {
    addr: std::net::SocketAddr,
    state: Arc<Mutex<ReceiverState>>,
}

#[derive(Default)]
struct ReceiverState {
    failures: Vec<StatusCode>,
    requests: usize,
    bodies: Vec<Value>,
}

impl Receiver {
    async fn start(mut failures: Vec<StatusCode>) -> Self {
        failures.reverse();
        let state = Arc::new(Mutex::new(ReceiverState { failures, ..ReceiverState::default() }));
        let app = axum::Router::new().route("/{*path}", post(accept)).with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app).into_future());
        Receiver { addr, state }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn bodies(&self) -> Vec<Value> {
        self.state.lock().unwrap().bodies.clone()
    }

    fn requests(&self) -> usize {
        self.state.lock().unwrap().requests
    }
}

async fn accept(State(state): State<Arc<Mutex<ReceiverState>>>, Json(body): Json<Value>) -> StatusCode {
    let mut state = state.lock().unwrap();
    state.requests += 1;
    if let Some(failure) = state.failures.pop() {
        return failure;
    }
    state.bodies.push(body);
    StatusCode::OK
}

/// Separate on-disk state for each caller, including table-driven test rows.
fn outbox_config() -> OutboxSection {
    OutboxSection {
        dir: crate::test_support::scratch_dir(&format!("alert-outbox-{}", rand::random::<u64>())),
        max_pages: 1024,
        max_bytes: 1_048_576,
        max_age: Duration::from_secs(3600),
    }
}

#[tokio::test]
async fn restart_delivers_committed_pages_in_route_order_with_their_original_episode_keys() {
    let cfg = outbox_config();
    let mut tasks = JoinSet::new();
    let outbox = Outbox::start(cfg.clone(), [true, true], Arc::new(Metrics::new()), &mut tasks).await.unwrap();
    for (market, severity) in [(1, Severity::Critical), (2, Severity::Warning), (3, Severity::Critical)] {
        let line = alert("offline", market);
        outbox.add(Page::new(line.clone(), line.key(), Status::Firing, severity)).await.unwrap();
    }
    drop(outbox);
    while tasks.join_next().await.is_some() {}
    let (critical, warning) = (Receiver::start(vec![]).await, Receiver::start(vec![]).await);
    let metrics = Arc::new(Metrics::new());
    let (_layer, router) = channel(routes(Some(&critical.url("/c")), Some(&warning.url("/w"))), metrics.clone());
    router.spawn(Duration::from_secs(60), &cfg).await.unwrap().drain(Duration::from_secs(10)).await;
    assert_eq!(
        critical.bodies().iter().map(|b| b["key"].as_str().unwrap()).collect::<Vec<_>>(),
        ["offline{market=1}", "offline{market=3}"]
    );
    assert_eq!(warning.bodies()[0]["key"], "offline{market=2}");
    assert!(critical.bodies().iter().all(|b| b["ts_ms"] == 1_700_000_000_000_i64));
    assert_eq!((pending(&metrics, "critical"), pending(&metrics, "warning")), (0, 0));
}

mod resolves;
