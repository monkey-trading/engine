//! A live worker retries refused synchronization before trusted-time recovery can charge financing.

use super::*;

use anyhow::{Context, ensure};
use engine_core::benchmark::BenchmarkHistory;

#[tokio::test]
async fn benchmark_worker_retries_after_time_halt_before_catch_up() {
    let dir = scratch_dir("benchmark-time-recovery");
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
            let _ = requests_tx.send(request);
            if request >= 3 {
                pending.notified().await;
            }
            "Effective Date,Rate Type,Rate (%)\n10/20/2026,SOFR,1.00\n10/21/2026,SOFR,1.50\n10/22/2026,SOFR,2.003\n"
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
    text.push_str(&format!(
        r#"
[benchmarks]
interval = "10ms"
timeout = "20s"
[[benchmarks.sources]]
publisher = "sofr"
endpoint = "{publisher}/sofr"
[[benchmarks.sources]]
publisher = "estr"
endpoint = "{publisher}/estr"
"#
    ));
    fs::write(&cfg_path, text).unwrap();
    // The sequencer's moving clock checks expiry authoritatively; the API clock stays test-local.
    let node = Node::start(&dir, START, std::sync::Arc::new(crate::clock::test_clock::TestClock::at(START))).await;
    let mut worker = None;
    let scenario = tokio::time::timeout(Duration::from_secs(10), async {
        node.sync(&cfg_path, || START).await;
        ensure!(requests.recv().await == Some(1), "initial publisher fetch completed");
        node.submit(Command::Deposit {
            account: TRADER,
            amount: fixed("100000"),
            chain_ref: "benchmark-recovery-deposit".into(),
        })
        .await;
        node.submit(Command::Prices {
            market: 1,
            quotes: ["venue-a", "venue-b"]
                .into_iter()
                .map(|s| SourceQuote { source: s.into(), mid: fixed("100000"), ts: START, contract: None })
                .collect(),
        })
        .await;
        let fills = node
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
        ensure!(fills.iter().any(|e| matches!(e, Event::Fill(_))), "position must fill");
        node.advance(FIRST - 30_000).await;
        ensure!(financing_state(&node).await?.0 == Some(FIRST), "pre-rollover sample is processed first");
        node.advance(FIRST + 1).await;
        ensure!(charges(&node.submit(Command::Tick).await) == [charge("0.01")], "initial dated rollover");
        let before = financing_state(&node).await?;

        // No fresh sample accompanies this advance: even a complete worker snapshot is refused.
        node.clock.advance(Duration::from_millis((RESTART - node.clock.now_ms()) as u64));
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
        ])?
        .args;
        let env = KeyEnv { key_file: None, key: Some(HexKey("42".repeat(32))) };
        worker = Some(tokio::spawn(admin::run_with_clock(args, env, || RESTART)));
        ensure!(requests.recv().await == Some(2), "worker fetches while trusted time is halted");
        // A sequential worker cannot start request three until the refused POST has returned.
        ensure!(requests.recv().await == Some(3), "worker retries its refused snapshot");
        ensure!(financing_state(&node).await? == before, "halted worker must append and charge nothing");

        node.tx
            .send(SeqRequest::TimeSample(sample_around(node.clock.as_ref())))
            .await
            .map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
        ensure!(node.try_submit(Command::Tick).await.is_none(), "ticker must wait for the blocked retry");
        ensure!(financing_state(&node).await? == before, "recovery must preserve pending financing and history");
        release.notify_one();
        ensure!(requests.recv().await == Some(4), "retry completes before the next worker attempt");
        let after = financing_state(&node).await?;
        ensure!(after.0 == Some(FIRST + 3 * 86_400_000), "both missed rollovers advance exactly once");
        ensure!(after.1 == before.1 - charge("0.015") - charge("0.02003"), "catch-up uses each dated rate");
        let tick = node.try_submit(Command::Tick).await.context("snapshot releases ordinary sequencing")?;
        ensure!(charges(&tick).is_empty(), "next tick must not charge catch-up again");
        anyhow::Ok(())
    })
    .await;

    let worker_result = if let Some(mut task) = worker {
        task.abort();
        let result = tokio::time::timeout(Duration::from_secs(3), &mut task).await;
        if result.is_err() {
            task.abort();
            let _ = task.await;
        }
        Some(result)
    } else {
        None
    };
    release.notify_waiters();
    let _ = shutdown.send(());
    let publisher_result = tokio::time::timeout(Duration::from_secs(5), &mut publishers).await;
    if publisher_result.is_err() {
        publishers.abort();
        let _ = publishers.await;
    }
    let Node { tx, mut task, api, .. } = node;
    api.abort();
    let _ = api.await;
    drop(tx);
    let node_result = tokio::time::timeout(Duration::from_secs(5), &mut task).await;
    if node_result.is_err() {
        task.abort();
        let _ = task.await;
    }

    scenario.expect("worker recovery completes within the publisher deadline").expect("worker recovery invariant");
    assert!(
        matches!(worker_result, Some(Ok(Err(err))) if err.is_cancelled()),
        "worker cancellation joins its pending publisher request"
    );
    publisher_result.expect("publisher shuts down").unwrap().unwrap();
    node_result.expect("sequencer drains").unwrap().unwrap();
    let entries = log_entries(&dir);
    let snapshots: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            let LogEntry::Command(c) = entry else { return None };
            let Command::SignedAdmin(request) = &c.sequenced.cmd else { return None };
            let AdminCommand::SetBenchmarkRates(batch) = &request.command else { return None };
            assert!(c.events.as_ref().unwrap().contains(&Event::AdminApplied));
            Some((request, batch))
        })
        .collect();
    assert_eq!(snapshots.len(), 2, "halted and pending attempts stay out of the public log");
    assert_eq!((snapshots[0].0.auth.nonce, snapshots[1].0.auth.nonce), (0, 1));
    assert_eq!(snapshots[1].1.as_of, "2026-10-22".parse().unwrap());
    assert_eq!(snapshots[1].1.checked.len(), 2);
    assert!(snapshots[1].1.checked.contains("SOFR") && snapshots[1].1.checked.contains("ESTR"));
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
    assert_eq!(replayed.engine.next_rollover(), Some(FIRST + 3 * 86_400_000));
}

/// Reads durable financing state without admitting a command or advancing the rollover schedule.
/// Error cases:
/// - the sequencer stops before serving the query
async fn financing_state(node: &Node) -> anyhow::Result<(Option<Millis>, Fixed, Option<u64>, BenchmarkHistory)> {
    let (reply, response) = oneshot::channel();
    node.tx
        .send(SeqRequest::Query(Box::new(move |engine| {
            let _ = reply.send((
                engine.next_rollover(),
                engine.accounts()[&TRADER].balance,
                engine.last_seq(),
                engine.benchmark_history().clone(),
            ));
        })))
        .await
        .map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
    response.await.context("financing state query")
}
