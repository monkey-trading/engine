//! V19 deterministic dev scenario through the real publisher worker, signed admin API, TLS, and log.

use super::*;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::http::StatusCode;
use clap::Parser;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::event::Event;
use engine_core::fixed::Rounding;
use engine_core::price::SourceQuote;
use engine_core::types::Side;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::admin::{self, AdminArgs, HexKey, KeyEnv};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::history::Reader;
use crate::limits::ApiLimits;
use crate::log::{LogEntry, Manifest};
use crate::sequencer::fixture_tests::replay_trusted as replay;
use crate::sequencer::{Request as SeqRequest, Sequencer, SequencerConfig};
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{dev_config, log_entries, sample_around, scratch_dir, sequencer_config};
use crate::tls::{TlsIdentity, serve};

const START: Millis = 1_792_504_800_000;
const FIRST: Millis = 1_792_530_000_000; // 20 Oct 2026 21:00 UTC = 22:00 London.
const RESTART: Millis = FIRST + 2 * 86_400_000 + 1;
const TRADER: AccountId = AccountId([0xaa; 20]);

mod recovery;
mod retention;
mod schedule;

type WatchCase = (&'static str, &'static str, fn() -> Millis, usize);

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    args: AdminArgs,
}

struct Node {
    tx: mpsc::Sender<SeqRequest>,
    task: JoinHandle<anyhow::Result<()>>,
    api: JoinHandle<anyhow::Result<()>>,
    url: String,
    pin: String,
    clock: Arc<TestClock>,
}

#[tokio::test]
async fn dev_scenario_benchmark_worker_catches_up_two_rollovers_at_their_dated_rates() {
    let dir = scratch_dir("benchmark-dev-scenario");
    fs::create_dir_all(&dir).unwrap();
    let (publisher, publishers) = publisher_server().await;
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
    let first = Node::start(&dir, START, || START).await;
    // A ticker cannot charge financing before the startup publisher synchronization.
    assert!(first.try_submit(Command::Tick).await.is_none());
    first.sync(&cfg_path, || START).await;
    first
        .submit(Command::Deposit { account: TRADER, amount: fixed("100000"), chain_ref: "benchmark-deposit".into() })
        .await;
    first
        .submit(Command::Prices {
            market: 1,
            quotes: ["venue-a", "venue-b"]
                .into_iter()
                .map(|s| SourceQuote { source: s.into(), mid: fixed("100000"), ts: START, contract: None })
                .collect(),
        })
        .await;
    let fills = first
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
    assert!(fills.iter().any(|e| matches!(e, Event::Fill(_))), "{fills:?}");
    // Renew trusted time before the rollover, then cross it within the sample grace.
    first.advance(FIRST - 30_000).await;
    first.advance(FIRST + 1).await;
    let events = first.submit(Command::Tick).await;
    assert_eq!(charges(&events), [charge("0.01")]);
    first.stop().await;

    // The publisher fixture includes the intervening rate changes. On restart it is fetched
    // before ticks, deposits, or feeds can cause catch-up to use the pre-downtime snapshot.
    let restarted = Node::start(&dir, RESTART, || RESTART).await;
    assert!(restarted.try_submit(Command::Tick).await.is_none());
    let request = restarted.sync(&cfg_path, || RESTART).await;
    assert!(request.command.is_benchmark_snapshot());
    let events = restarted.submit(Command::Tick).await;
    assert!(charges(&events).is_empty(), "snapshot already charged both missed rollovers");
    restarted.stop().await;
    publishers.abort();

    let entries = log_entries(&dir);
    let financing: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(c) => c.events.as_deref(),
            _ => None,
        })
        .flat_map(charges)
        .collect();
    assert_eq!(financing, [charge("0.01"), charge("0.015"), charge("0.02003")]);
    let replayed = replay(&entries).unwrap();
    assert_eq!(replayed.engine.accounts()[&TRADER].balance, fixed("100000") - financing.into_iter().sum::<Fixed>());
    assert!(entries.iter().any(|entry| matches!(entry, LogEntry::Command(c) if c.sequenced.cmd == Command::SignedAdmin(Box::new(request.clone())))));
}

#[tokio::test]
async fn benchmark_watch_retries_and_prefetches_before_rollover_then_cancels_in_flight() {
    let cases: [WatchCase; 2] = [("periodic", "10ms", || START, 3), ("preflight", "24h", || FIRST - 34_000 - 50, 2)];
    run_watch_cases(&cases, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn benchmark_watch_interrupts_inflight_fetch_after_one_sigint() {
    const CHILD: &str = "ENGINE_GH33_BENCHMARK_SIGINT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        run_watch_cases(&[("sigint", "24h", || FIRST - 50_000 - 50, 2)], true).await;
        return;
    }
    let child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "api::benchmark_tests::benchmark_watch_interrupts_inflight_fetch_after_one_sigint",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .kill_on_drop(true)
        .output();
    let output =
        tokio::time::timeout(Duration::from_secs(15), child).await.expect("isolated SIGINT test completes").unwrap();
    assert!(
        output.status.success(),
        "SIGINT child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs bounded publisher retries/preflight, then cancels a pending request by task or SIGINT.
async fn run_watch_cases(cases: &[WatchCase], interrupt: bool) {
    for &(name, interval, clock, pending_request) in cases {
        let dir = scratch_dir(&format!("benchmark-watch-{name}"));
        fs::create_dir_all(&dir).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let publisher = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let (requests_tx, mut requests) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let pending = release.clone();
        let sofr = move || {
            let calls = calls.clone();
            let requests_tx = requests_tx.clone();
            let pending = pending.clone();
            async move {
                let request = calls.fetch_add(1, Ordering::SeqCst) + 1;
                requests_tx.send(request).unwrap();
                if request == 1 && pending_request == 3 {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "publisher unavailable");
                }
                if request >= pending_request {
                    pending.notified().await;
                }
                (StatusCode::OK, "Effective Date,Rate Type,Rate (%)\n10/20/2026,SOFR,1.00\n")
            }
        };
        let app = Router::new()
            .route("/sofr", get(sofr))
            .route("/estr", get(|| async { "KEY,TIME_PERIOD,OBS_VALUE\nEST.B.EU000A2X2A25.WT,2026-10-20,2.442\n" }));
        let (shutdown, stopped) = oneshot::channel();
        let mut publishers = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        let cfg_path = dir.join("node.toml");
        let mut text = fs::read_to_string(crate::test_support::DEV_CONFIG).unwrap();
        let publisher_timeout = if interrupt { "10s" } else { "2s" };
        text.push_str(&format!(
            r#"
    [benchmarks]
    interval = "{interval}"
    timeout = "{publisher_timeout}"
    [[benchmarks.sources]]
    publisher = "sofr"
    endpoint = "{publisher}/sofr"
    [[benchmarks.sources]]
    publisher = "estr"
    endpoint = "{publisher}/estr"
    "#
        ));
        fs::write(&cfg_path, text).unwrap();
        let node = Node::start(&dir, clock(), clock).await;
        let args = Cli::try_parse_from([
            "engine",
            "--config",
            cfg_path.to_str().unwrap(),
            "--node",
            &node.url,
            "--pin",
            &node.pin,
            "sync-benchmarks",
            "--watch",
        ])
        .unwrap()
        .args;
        let env = KeyEnv { key_file: None, key: Some(HexKey("42".repeat(32))) };
        let mut worker = tokio::spawn(admin::run_with_clock(args, env, clock));
        let observed = tokio::time::timeout(Duration::from_secs(10), async {
            let mut seen = Vec::new();
            for _ in 0..pending_request {
                seen.push(requests.recv().await);
            }
            seen
        })
        .await;
        let signal_result = if interrupt {
            Some(
                tokio::process::Command::new("/bin/kill")
                    .args(["-INT", &std::process::id().to_string()])
                    .status()
                    .await,
            )
        } else {
            worker.abort();
            None
        };
        let worker_result = tokio::time::timeout(Duration::from_secs(3), &mut worker).await;
        if worker_result.is_err() {
            worker.abort();
            let _ = worker.await;
        }
        release.notify_one();
        let _ = shutdown.send(());
        let publisher_result = tokio::time::timeout(Duration::from_secs(5), &mut publishers).await;
        if publisher_result.is_err() {
            publishers.abort();
            let _ = publishers.await;
        }
        node.stop().await;

        let expected: Vec<_> = (1..=pending_request).map(Some).collect();
        assert_eq!(observed.expect("watch reaches its pending fetch"), expected, "{name}");
        let returned = if let Some(sent) = signal_result {
            assert!(sent.unwrap().success(), "the child sends exactly one SIGINT to itself");
            Some(worker_result.expect("SIGINT interrupts the in-flight fetch before its timeout").unwrap().unwrap())
        } else {
            assert!(
                matches!(worker_result, Ok(Err(err)) if err.is_cancelled()),
                "worker must remain in its pending fetch"
            );
            None
        };
        publisher_result.expect("publisher shutdown completes").unwrap().unwrap();
        let entries = log_entries(&dir);
        let snapshots: Vec<_> = entries
            .iter()
            .filter_map(|entry| {
                let LogEntry::Command(c) = entry else { return None };
                let Command::SignedAdmin(request) = &c.sequenced.cmd else { return None };
                let AdminCommand::SetBenchmarkRates(batch) = &request.command else { return None };
                assert!(c.events.as_ref().unwrap().contains(&Event::AdminApplied));
                Some(batch)
            })
            .collect();
        assert_eq!(snapshots.len(), pending_request - 1, "only completed snapshots may be sequenced: {name}");
        if let Some(request) = returned {
            assert!(entries.iter().any(|entry| matches!(entry, LogEntry::Command(c) if c.sequenced.cmd == Command::SignedAdmin(Box::new(request.clone())))), "SIGINT returns the last successfully sequenced snapshot");
        }
        for batch in &snapshots {
            assert_eq!(batch.checked.len(), 2);
            assert!(batch.checked.contains("SOFR") && batch.checked.contains("ESTR"));
        }
        if pending_request == 3 {
            assert!(snapshots[0].rates.iter().all(|rate| rate.benchmark != "SOFR"));
        }
        assert!(snapshots[0].rates.iter().any(|rate| rate.benchmark == "ESTR"));
        let sofr: Vec<_> = snapshots.last().unwrap().rates.iter().filter(|rate| rate.benchmark == "SOFR").collect();
        assert_eq!(sofr.len(), 1);
        let day = "2026-10-20".parse().unwrap();
        assert_eq!(sofr[0].effective_date, day);
        assert_eq!(sofr[0].annual_rate, fixed("0.01"));
        let replayed = replay(&entries).unwrap();
        assert_eq!(replayed.engine.benchmark_history()["SOFR"][&day], fixed("0.01"));
    }
}

impl Node {
    async fn start(dir: &Path, at: Millis, api_clock: fn() -> Millis) -> Self {
        Self::start_with_router(dir, at, api_clock, std::convert::identity).await
    }

    async fn start_with_router(
        dir: &Path,
        at: Millis,
        api_clock: fn() -> Millis,
        wrap: impl FnOnce(Router) -> Router,
    ) -> Self {
        let clock = Arc::new(TestClock::at(at));
        let key = Arc::new(EnclaveKey::test_key());
        let (stream, _) = broadcast::channel(1024);
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let (withdrawals, _) = mpsc::unbounded_channel();
        let cfg = SequencerConfig { clock: clock.clone(), ..sequencer_config(dir, manifest_tx) };
        let operator = cfg.operator;
        let mut seq = Sequencer::recover(cfg, key.clone(), stream.clone(), withdrawals).unwrap();
        seq.wait_for_benchmarks().unwrap();
        seq.activate_legacy_protocol().unwrap();
        if seq.needs_genesis() {
            seq.genesis(&dev_config().genesis).unwrap();
        }
        seq.trust(sample_around(clock.as_ref())).unwrap();
        let (tx, rx) = mpsc::channel(1024);
        let task = tokio::spawn(seq.run(rx));
        let tls = TlsIdentity::generate().unwrap();
        let bundle = AttestationBundle {
            enclave_address: key.address(),
            config_sha256: [0; 32],
            tls_public_key_sha256: tls.public_key_sha256,
        };
        let state = ApiState {
            protocol: engine_core::protocol::ProtocolVersion::V23,
            access: None,
            tx: tx.clone(),
            stream,
            attestation: Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap()),
            auth: ClientAuth::Unsigned,
            operator,
            mock_custody: true,
            payouts_enabled: true,
            clock: api_clock,
            log_dir: dir.into(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: Arc::new(ApiLimits::new(&dev_config().api)),
            history: Reader::new(PathBuf::new(), watch::channel(true).1),
            emission_budget: None,
            listing: Default::default(),
            signing: crate::test_support::signing(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let app = wrap(router(state.clone()));
        let api = tokio::spawn(serve(listener, tls.config, app, state.limits.clone(), dev_config().api));
        Self { tx, task, api, url, pin: hex::encode(tls.public_key_sha256), clock }
    }

    async fn sync(&self, config: &Path, clock: fn() -> Millis) -> engine_core::command::AdminRequest {
        let args = Cli::try_parse_from([
            "engine",
            "--config",
            config.to_str().unwrap(),
            "--node",
            &self.url,
            "--pin",
            &self.pin,
            "sync-benchmarks",
        ])
        .unwrap()
        .args;
        let env = KeyEnv { key_file: None, key: Some(HexKey("42".repeat(32))) };
        admin::run_with_clock(args, env, clock).await.unwrap()
    }

    async fn try_submit(&self, cmd: Command) -> Option<Vec<Event>> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(SeqRequest::Submit { cmd, reply: Some(reply) }).await.unwrap();
        rx.await.ok()
    }

    async fn submit(&self, cmd: Command) -> Vec<Event> {
        self.try_submit(cmd).await.expect("sequenced command")
    }

    async fn advance(&self, to: Millis) {
        self.clock.advance(Duration::from_millis((to - self.clock.now_ms()) as u64));
        self.tx.send(SeqRequest::TimeSample(sample_around(self.clock.as_ref()))).await.unwrap();
        let (reply, seen) = oneshot::channel();
        self.tx
            .send(SeqRequest::Query(Box::new(move |_| {
                let _ = reply.send(());
            })))
            .await
            .unwrap();
        seen.await.unwrap();
    }

    async fn stop(self) {
        self.api.abort();
        let _ = self.api.await;
        drop(self.tx);
        self.task.await.unwrap().unwrap();
    }
}

async fn publisher_server() -> (String, JoinHandle<std::io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/sofr",
            get(|| async {
                "Effective Date,Rate Type,Rate (%)\n10/20/2026,SOFR,1.00\n10/21/2026,SOFR,1.50\n10/22/2026,SOFR,2.003\n"
            }),
        )
        .route("/estr", get(|| async { "KEY,TIME_PERIOD,OBS_VALUE\nEST.B.EU000A2X2A25.WT,2026-10-20,2.442\n" }));
    (url, tokio::spawn(async move { axum::serve(listener, app).await }))
}

fn fixed(s: &str) -> Fixed {
    s.parse().unwrap()
}
fn charge(rate: &str) -> Fixed {
    fixed("10000").mul_rounded(fixed(rate) + Fixed::from_bps(500), Rounding::Up).div_int_rounded(365, Rounding::Up)
}
fn charges(events: &[Event]) -> Vec<Fixed> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Financing { amount, market: 1, .. } => Some(*amount),
            _ => None,
        })
        .collect()
}
