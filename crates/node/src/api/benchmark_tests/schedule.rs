//! Legal rollover changes must invalidate cached worker deadlines and in-flight observations.

use super::*;

use std::sync::atomic::AtomicBool;

use anyhow::{Context, ensure};
use engine_core::params::ProtocolParams;
use k256::ecdsa::SigningKey;

use crate::admin::NodeApi;

const EARLY: Millis = START - 3 * 3_600_000; // 20 October 2026 11:00 UTC.
const MOVED: Millis = EARLY + 2 * 3_600_000; // London 14:00 / Tokyo 22:00.

mod reconnect;

struct PublisherProbe {
    version: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
    blocked: Arc<AtomicUsize>,
    release: Arc<Notify>,
    requests: mpsc::UnboundedReceiver<usize>,
    task: JoinHandle<std::io::Result<()>>,
}

struct Scenario {
    dir: PathBuf,
    config: PathBuf,
    node: Node,
    publisher: PublisherProbe,
    worker: Option<JoinHandle<anyhow::Result<engine_core::command::AdminRequest>>>,
}

#[tokio::test]
async fn dev_scenario_benchmark_worker_refreshes_after_earlier_rollover_hour() {
    changed_schedule(false).await;
}

#[tokio::test]
async fn dev_scenario_benchmark_worker_refreshes_after_earlier_rollover_timezone() {
    changed_schedule(true).await;
}

/// The new two-hour deadline must not sleep behind the old 24-hour polling interval.
async fn changed_schedule(timezone: bool) {
    let mut scenario = Scenario::start(&format!("benchmark-reschedule-{timezone}"), EARLY, || EARLY).await;
    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
        scenario.watch();
        scenario.next_request(2).await?;
        wait_nonce(&scenario.node, 2).await?;
        scenario.publisher.version.store(2, Ordering::SeqCst);
        let mut params = dev_config().genesis.params;
        if timezone {
            params.rollover.tz = chrono_tz::Asia::Tokyo;
        } else {
            params.rollover.hour = 14;
        }
        ensure!(params.rollover.next_after(EARLY) == MOVED, "legal schedule selects the earlier rollover");
        ensure!(signed_params(&scenario.node, params).await?.contains(&Event::AdminApplied), "schedule applies");
        scenario.next_request(3).await?;
        wait_nonce(&scenario.node, 4).await?;
        scenario.node.advance(MOVED - 30_000).await;
        scenario.node.advance(MOVED + 1).await;
        let events = scenario.node.submit(Command::Tick).await;
        ensure!(charges(&events) == [charge("0.02")], "moved rollover charges the intervening publication");
        ensure!(balance(&scenario.node).await? == fixed("100000") - charge("0.02"), "financing conserves money");
        anyhow::Ok(())
    })
    .await;
    let dir = scenario.finish().await;
    outcome.expect("earlier schedule refresh completes promptly").expect("earlier schedule financial invariant");
    assert_logged_financing(&dir, "0.02", Some(3));
}

#[tokio::test]
async fn benchmark_worker_discards_inflight_snapshot_after_coalesced_schedule_changes() {
    let mut scenario = Scenario::start("benchmark-reschedule-inflight", EARLY, || EARLY).await;
    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
        scenario.publisher.version.store(2, Ordering::SeqCst);
        scenario.publisher.blocked.store(2, Ordering::SeqCst);
        scenario.watch();
        scenario.next_request(2).await?;
        let original = dev_config().genesis.params;
        let mut moved = original.clone();
        moved.rollover.tz = chrono_tz::Asia::Tokyo;
        ensure!(signed_params(&scenario.node, moved).await?.contains(&Event::AdminApplied), "A to B applies");
        ensure!(signed_params(&scenario.node, original).await?.contains(&Event::AdminApplied), "B to A applies");
        scenario.publisher.version.store(3, Ordering::SeqCst);
        scenario.publisher.release.notify_one();
        scenario.next_request(3).await?;
        wait_nonce(&scenario.node, 4).await?;
        scenario.node.advance(FIRST - 30_000).await;
        scenario.node.advance(FIRST + 1).await;
        ensure!(charges(&scenario.node.submit(Command::Tick).await) == [charge("0.03")], "latest attempt wins");
        ensure!(balance(&scenario.node).await? == fixed("100000") - charge("0.03"), "stale fetch charges nothing");
        anyhow::Ok(())
    })
    .await;
    let dir = scenario.finish().await;
    outcome.expect("coalesced changes complete promptly").expect("in-flight schedule fencing invariant");
    assert_logged_financing(&dir, "0.03", None);
    let entries = log_entries(&dir);
    assert!(
        entries.iter().all(|entry| {
            let LogEntry::Command(command) = entry else { return true };
            if !command.events.as_ref().expect("public command events").contains(&Event::AdminApplied) {
                return true;
            }
            let Command::SignedAdmin(request) = &command.sequenced.cmd else { return true };
            let AdminCommand::SetBenchmarkRates(batch) = &request.command else { return true };
            batch.rates.iter().all(|rate| rate.benchmark != "SOFR" || rate.annual_rate != fixed("0.02"))
        }),
        "the nonce acquired before the stale fetch prevents even temporary acceptance of its observation"
    );
}

#[tokio::test]
async fn benchmark_worker_attempts_late_schedule_and_holds_due_financing_until_failed_batch_completes() {
    for (name, at, api_clock) in [
        ("inside-window", MOVED - 10_000, (|| MOVED - 10_000) as fn() -> Millis),
        ("last-millisecond", MOVED - 1, (|| MOVED - 1) as fn() -> Millis),
    ] {
        let mut scenario = Scenario::start(name, at, api_clock).await;
        let outcome = tokio::time::timeout(Duration::from_secs(15), async {
            scenario.watch();
            scenario.next_request(2).await?;
            wait_nonce(&scenario.node, 2).await?;
            scenario.publisher.failed.store(true, Ordering::SeqCst);
            scenario.publisher.blocked.store(3, Ordering::SeqCst);
            let mut params = dev_config().genesis.params;
            params.rollover.hour = 14;
            ensure!(signed_params(&scenario.node, params).await?.contains(&Event::AdminApplied), "late change applies");
            scenario.next_request(3).await?;
            scenario.node.advance(MOVED + 1).await;
            ensure!(
                scenario.node.try_submit(Command::Tick).await.is_none(),
                "due financing waits for the actual attempt"
            );
            ensure!(balance(&scenario.node).await? == fixed("100000"), "pending attempt cannot charge an older rate");
            scenario.publisher.release.notify_one();
            wait_nonce(&scenario.node, 4).await?;
            ensure!(
                balance(&scenario.node).await? == fixed("100000") - charge("0.01"),
                "complete empty batch retains dated policy"
            );
            ensure!(charges(&scenario.node.submit(Command::Tick).await).is_empty(), "completed catch-up charges once");
            anyhow::Ok(())
        })
        .await;
        let dir = scenario.finish().await;
        outcome.expect("late schedule attempt completes promptly").expect("due financing synchronization invariant");
        assert_logged_financing(&dir, "0.01", Some(3));
        let entries = log_entries(&dir);
        let empty = entries
            .iter()
            .filter_map(|entry| {
                let LogEntry::Command(command) = entry else { return None };
                let Command::SignedAdmin(request) = &command.sequenced.cmd else { return None };
                let AdminCommand::SetBenchmarkRates(batch) = &request.command else { return None };
                batch.rates.is_empty().then_some(batch)
            })
            .collect::<Vec<_>>();
        assert_eq!(empty.len(), 1, "only the completed failed fetch releases financing");
        assert!(empty[0].checked.contains("SOFR") && empty[0].checked.contains("ESTR"));
    }
}

#[tokio::test]
async fn benchmark_worker_does_not_refresh_for_unchanged_rejected_or_unrelated_parameters() {
    let mut scenario = Scenario::start("benchmark-reschedule-neighbors", EARLY, || EARLY).await;
    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
        scenario.watch();
        scenario.next_request(2).await?;
        wait_nonce(&scenario.node, 2).await?;
        let params = dev_config().genesis.params;
        ensure!(
            signed_params(&scenario.node, params.clone()).await?.contains(&Event::AdminApplied),
            "same schedule applies"
        );
        let mut unrelated = params.clone();
        unrelated.profit_fee_bps += 1;
        ensure!(
            signed_params(&scenario.node, unrelated).await?.contains(&Event::AdminApplied),
            "unrelated setting applies"
        );
        let mut rejected = params;
        rejected.rollover.hour = 24;
        ensure!(
            signed_params(&scenario.node, rejected)
                .await?
                .iter()
                .any(|event| matches!(event, Event::AdminRejected { .. })),
            "invalid schedule rejects"
        );
        ensure!(
            tokio::time::timeout(Duration::from_millis(100), scenario.publisher.requests.recv()).await.is_err(),
            "unchanged effective schedule does not initiate another publisher fetch"
        );
        ensure!(balance(&scenario.node).await? == fixed("100000"), "neighbors do not charge financing");
        anyhow::Ok(())
    })
    .await;
    let dir = scenario.finish().await;
    outcome.expect("schedule neighbors complete promptly").expect("unchanged schedule invariant");
    assert!(
        log_entries(&dir)
            .iter()
            .filter_map(|entry| match entry {
                LogEntry::Command(c) => c.events.as_deref(),
                _ => None,
            })
            .flat_map(charges)
            .next()
            .is_none()
    );
    replay(&log_entries(&dir)).expect("all signed neighbor commands replay");
}

impl Scenario {
    /// Starts actual pinned HTTPS administration and a position backed by a dated 1% snapshot.
    async fn start(name: &str, at: Millis, api_clock: fn() -> Millis) -> Self {
        Self::start_custom(name, at, api_clock, "2s", std::convert::identity).await
    }

    async fn start_custom(
        name: &str,
        at: Millis,
        api_clock: fn() -> Millis,
        publisher_timeout: &str,
        wrap: impl FnOnce(Router) -> Router,
    ) -> Self {
        let dir = scratch_dir(name);
        fs::create_dir_all(&dir).unwrap();
        let (endpoint, publisher) = publisher_probe().await;
        let config = dir.join("node.toml");
        let mut text = fs::read_to_string(crate::test_support::DEV_CONFIG).unwrap();
        text.push_str(&format!(
            r#"
[benchmarks]
interval = "24h"
timeout = "{publisher_timeout}"
[[benchmarks.sources]]
publisher = "sofr"
endpoint = "{endpoint}/sofr"
[[benchmarks.sources]]
publisher = "estr"
endpoint = "{endpoint}/estr"
"#
        ));
        fs::write(&config, text).unwrap();
        let node = Node::start_with_router(&dir, at, api_clock, wrap).await;
        node.sync(&config, api_clock).await;
        node.submit(Command::Deposit {
            account: TRADER,
            amount: fixed("100000"),
            chain_ref: "schedule-deposit".into(),
        })
        .await;
        node.submit(Command::Prices {
            market: 1,
            quotes: ["venue-a", "venue-b"]
                .into_iter()
                .map(|source| SourceQuote { source: source.into(), mid: fixed("100000"), ts: at, contract: None })
                .collect(),
        })
        .await;
        let events = node
            .submit(Command::PlaceOrder(OrderRequest {
                account: TRADER,
                client_id: 1,
                market: 1,
                side: Side::Buy,
                qty: fixed("0.1"),
                kind: OrderKind::Market { quoted: fixed("100000"), max_slippage_bps: None },
                reduce_only: false,
                auth: None,
            }))
            .await;
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "schedule position fills");
        let mut scenario = Self { dir, config, node, publisher, worker: None };
        scenario.next_request(1).await.unwrap();
        scenario
    }

    /// Starts the production watch client with the node's explicitly injected moving clock.
    fn watch(&mut self) {
        let args = Cli::try_parse_from([
            "engine",
            "--config",
            self.config.to_str().unwrap(),
            "--node",
            &self.node.url,
            "--pin",
            &self.node.pin,
            "sync-benchmarks",
            "--watch",
        ])
        .unwrap()
        .args;
        let clock = self.node.clock.clone();
        self.worker = Some(tokio::spawn(admin::run_with_clock(
            args,
            KeyEnv { key_file: None, key: Some(HexKey("42".repeat(32))) },
            move || clock.now_ms(),
        )));
    }

    /// Waits for a specific real publisher request, so a sleep cannot fake synchronization.
    /// Error cases:
    /// - the publisher exits or a different attempt is observed
    async fn next_request(&mut self, expected: usize) -> anyhow::Result<()> {
        ensure!(self.publisher.requests.recv().await == Some(expected), "publisher request {expected} must occur");
        Ok(())
    }

    /// Joins canceled work before reading the durable public log.
    async fn finish(mut self) -> PathBuf {
        if let Some(worker) = self.worker.take() {
            worker.abort();
            let _ = worker.await;
        }
        self.publisher.release.notify_waiters();
        self.publisher.task.abort();
        let _ = self.publisher.task.await;
        tokio::time::timeout(Duration::from_secs(5), self.node.stop()).await.expect("schedule node drains");
        self.dir
    }
}

/// Serves bounded official-shaped publisher responses, capturing observations before an optional pause.
async fn publisher_probe() -> (String, PublisherProbe) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let version = Arc::new(AtomicUsize::new(1));
    let failed = Arc::new(AtomicBool::new(false));
    let blocked = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let sofr = {
        let (version, failed, blocked, release) = (version.clone(), failed.clone(), blocked.clone(), release.clone());
        move || {
            let (version, failed, blocked, release, calls, requests_tx) =
                (version.clone(), failed.clone(), blocked.clone(), release.clone(), calls.clone(), requests_tx.clone());
            async move {
                let request = calls.fetch_add(1, Ordering::SeqCst) + 1;
                let rate = version.load(Ordering::SeqCst);
                let failure = failed.load(Ordering::SeqCst);
                let _ = requests_tx.send(request);
                if request == blocked.load(Ordering::SeqCst) {
                    release.notified().await;
                }
                if failure {
                    (StatusCode::SERVICE_UNAVAILABLE, "publisher unavailable".into())
                } else {
                    (StatusCode::OK, format!("Effective Date,Rate Type,Rate (%)\n10/20/2026,SOFR,{rate}.00\n"))
                }
            }
        }
    };
    let estr = {
        let failed = failed.clone();
        move || {
            let failed = failed.clone();
            async move {
                if failed.load(Ordering::SeqCst) {
                    (StatusCode::SERVICE_UNAVAILABLE, "publisher unavailable")
                } else {
                    (StatusCode::OK, "KEY,TIME_PERIOD,OBS_VALUE\nEST.B.EU000A2X2A25.WT,2026-10-20,2.442\n")
                }
            }
        }
    };
    let app = Router::new().route("/sofr", get(sofr)).route("/estr", get(estr));
    let task = tokio::spawn(async move { axum::serve(listener, app).await });
    (endpoint, PublisherProbe { version, failed, blocked, release, requests, task })
}

/// Submits even invalid parameter content through the real authenticated operator route.
/// Error cases:
/// - querying, signing configuration, TLS, or submission fails
async fn signed_params(node: &Node, params: ProtocolParams) -> anyhow::Result<Vec<Event>> {
    let operator = dev_config().operator()?;
    let (reply, response) = oneshot::channel();
    node.tx
        .send(SeqRequest::Query(Box::new(move |engine| {
            let _ = reply.send(engine.next_request_nonce(operator.address));
        })))
        .await
        .map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
    let nonce = response.await.context("operator nonce")?;
    let key = SigningKey::from_slice(&[0x42; 32])?;
    let request = admin::sign(&key, &operator, AdminCommand::SetParams(params), nonce, node.clock.now_ms() + 150_000);
    admin::submit(
        &NodeApi { url: node.url.parse()?, pin: hex::decode(&node.pin)?.try_into().expect("SHA-256 pin") },
        &request,
    )
    .await
}

/// Waits until the snapshot's signed nonce proves the publisher result was accepted.
/// Error cases:
/// - the sequencer stops before serving the query
async fn wait_nonce(node: &Node, expected: u64) -> anyhow::Result<()> {
    let operator = dev_config().operator()?;
    loop {
        let (reply, response) = oneshot::channel();
        node.tx
            .send(SeqRequest::Query(Box::new(move |engine| {
                let _ = reply.send(engine.next_request_nonce(operator.address));
            })))
            .await
            .map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
        if response.await.context("accepted snapshot nonce")? >= expected {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Queries the account without admitting a financing-triggering command.
/// Error cases:
/// - the sequencer stops before serving the query
async fn balance(node: &Node) -> anyhow::Result<Fixed> {
    let (reply, response) = oneshot::channel();
    node.tx
        .send(SeqRequest::Query(Box::new(move |engine| {
            let _ = reply.send(engine.accounts()[&TRADER].balance);
        })))
        .await
        .map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
    response.await.context("account balance")
}

/// Checks exact financing, signed ordering, and independently trusted public-log replay.
fn assert_logged_financing(dir: &Path, rate: &str, expected_snapshots: Option<usize>) {
    let entries = log_entries(dir);
    let mut snapshots = 0;
    let mut latest = None;
    let mut financing = Vec::new();
    for entry in &entries {
        let LogEntry::Command(command) = entry else { continue };
        let events = command.events.as_ref().expect("public command events");
        if let Command::SignedAdmin(request) = &command.sequenced.cmd
            && let AdminCommand::SetBenchmarkRates(batch) = &request.command
            && events.contains(&Event::AdminApplied)
        {
            assert!(batch.checked.contains("SOFR") && batch.checked.contains("ESTR"));
            snapshots += 1;
            if let Some(observation) = batch.rates.iter().find(|observation| observation.benchmark == "SOFR") {
                latest = Some(observation.annual_rate);
            }
        }
        for amount in charges(events) {
            assert_eq!(latest, Some(fixed(rate)), "fresh signed observation precedes financing in the public log");
            financing.push(amount);
        }
    }
    assert_eq!(financing, [charge(rate)]);
    if let Some(expected) = expected_snapshots {
        assert_eq!(snapshots, expected);
    } else {
        assert!(snapshots >= 2, "initial and post-change snapshots must both be public");
    }
    let replayed = replay(&entries).expect("trusted signed log replay");
    assert_eq!(replayed.engine.accounts()[&TRADER].balance, fixed("100000") - charge(rate));
}
