//! Publisher failures must preserve dates made selectable by a legal calendar change.

use super::*;

use std::sync::atomic::AtomicBool;

use anyhow::{Context, ensure};
use engine_core::benchmark::BenchmarkHistory;
use engine_core::command::AdminRequest;

const COLD: Millis = START - 8 * 3_600_000; // 20 October 2026 06:00 UTC.
const REWOUND: Millis = COLD + 3_600_000; // Honolulu 19 October 21:00.
const RESUMED: Millis = REWOUND + 1;

#[tokio::test]
async fn benchmark_worker_retains_rewound_calendar_predecessor_after_restart_and_failed_fetch() {
    run_retention_case(true).await;
}

#[tokio::test]
async fn benchmark_worker_retains_rewound_calendar_predecessor_after_time_halt_and_failed_fetch() {
    run_retention_case(false).await;
}

async fn run_retention_case(restart: bool) {
    let dir = scratch_dir(&format!("benchmark-calendar-retention-{restart}"));
    fs::create_dir_all(&dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let publisher = format!("http://{}", listener.local_addr().unwrap());
    let failed = Arc::new(AtomicBool::new(false));
    let sofr_failed = failed.clone();
    let estr_failed = failed.clone();
    let app = Router::new()
        .route(
            "/sofr",
            get(move || {
                let failed = sofr_failed.clone();
                async move {
                    if failed.load(Ordering::SeqCst) {
                        (StatusCode::SERVICE_UNAVAILABLE, "publisher unavailable")
                    } else {
                        (
                            StatusCode::OK,
                            "Effective Date,Rate Type,Rate (%)\n10/19/2026,SOFR,1.00\n10/20/2026,SOFR,2.00\n",
                        )
                    }
                }
            }),
        )
        .route(
            "/estr",
            get(move || {
                let failed = estr_failed.clone();
                async move {
                    if failed.load(Ordering::SeqCst) {
                        (StatusCode::SERVICE_UNAVAILABLE, "publisher unavailable")
                    } else {
                        (StatusCode::OK, "KEY,TIME_PERIOD,OBS_VALUE\nEST.B.EU000A2X2A25.WT,2026-10-19,2.442\n")
                    }
                }
            }),
        );
    let (shutdown, stopped) = oneshot::channel();
    let mut publishers = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let config = dir.join("node.toml");
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
    fs::write(&config, text).unwrap();
    let mut node = Some(Node::start(&dir, COLD, || COLD).await);
    let scenario = tokio::time::timeout(Duration::from_secs(15), async {
        let first = node.as_ref().unwrap();
        run_admin(first, &config, &["sync-benchmarks"], || COLD).await?;
        first
            .submit(Command::Deposit {
                account: TRADER,
                amount: fixed("100000"),
                chain_ref: "calendar-retention-deposit".into(),
            })
            .await;
        first
            .submit(Command::Prices {
                market: 1,
                quotes: ["venue-a", "venue-b"]
                    .into_iter()
                    .map(|source| SourceQuote { source: source.into(), mid: fixed("100000"), ts: COLD, contract: None })
                    .collect(),
            })
            .await;
        let events = first
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
        ensure!(events.iter().any(|event| matches!(event, Event::Fill(_))), "retention position must fill");
        let params_path = dir.join("params.toml");
        let mut params = dev_config().genesis.params;
        params.rollover.tz = chrono_tz::Pacific::Honolulu;
        params.rollover.hour = 21;
        params.rollover.minute = 0;
        fs::write(&params_path, toml::to_string(&params)?)?;
        run_admin(first, &config, &["set-params", params_path.to_str().context("params path")?], || COLD).await?;
        let before = retention_state(first).await?;
        ensure!(before.0 == Some(REWOUND), "legal calendar change selects the previous local date");
        failed.store(true, Ordering::SeqCst);
        if restart {
            node.take().unwrap().stop().await;
            node = Some(Node::start(&dir, RESUMED, || RESUMED).await);
        } else {
            let live = node.as_ref().unwrap();
            live.clock.advance(Duration::from_millis((RESUMED - live.clock.now_ms()) as u64));
            let refused = run_admin(live, &config, &["sync-benchmarks"], || RESUMED).await;
            ensure!(refused.is_err(), "complete failed fetch cannot bypass expired trusted time");
            ensure!(retention_state(live).await? == before, "refused snapshot changes no financing or history");
            live.advance(RESUMED).await;
        }
        let live = node.as_ref().unwrap();
        ensure!(live.try_submit(Command::Tick).await.is_none(), "recovery waits for a complete publisher attempt");
        let request = run_admin(live, &config, &["sync-benchmarks"], || RESUMED).await?;
        let AdminCommand::SetBenchmarkRates(batch) = &request.command else { unreachable!() };
        ensure!(batch.rates.is_empty(), "failed publishers supply no replacement observations");
        ensure!(batch.checked.contains("SOFR") && batch.checked.contains("ESTR"), "both fetches were attempted");
        ensure!(
            batch.as_of == "2026-10-19".parse::<chrono::NaiveDate>()?,
            "snapshot follows the rewound local calendar"
        );
        let after = retention_state(live).await?;
        ensure!(after.1 == before.1 - charge("0.01"), "rewound financing uses the retained dated predecessor");
        ensure!(after.0 == Some(REWOUND + 86_400_000), "rewound rollover advances exactly once");
        ensure!(charges(&live.submit(Command::Tick).await).is_empty(), "catch-up cannot charge twice");

        // Restore London: the later observation must still serve its own date and prune normally.
        params.rollover = dev_config().genesis.params.rollover;
        fs::write(&params_path, toml::to_string(&params)?)?;
        run_admin(live, &config, &["set-params", params_path.to_str().context("params path")?], || RESUMED).await?;
        let refresh = run_admin(live, &config, &["sync-benchmarks"], || RESUMED).await?;
        let AdminCommand::SetBenchmarkRates(batch) = &refresh.command else { unreachable!() };
        ensure!(batch.rates.is_empty(), "the restored calendar still attempts both failed publishers");
        ensure!(batch.checked.contains("SOFR") && batch.checked.contains("ESTR"), "both fetches were attempted");
        ensure!(batch.as_of == "2026-10-20".parse::<chrono::NaiveDate>()?, "refresh uses restored London's date");
        ensure!(retention_state(live).await?.1 == after.1, "a predeadline refresh charges nothing");
        live.advance(FIRST - 30_000).await;
        live.advance(FIRST + 1).await;
        ensure!(
            charges(&live.submit(Command::Tick).await) == [charge("0.02")],
            "later date uses the later observation"
        );
        let final_state = retention_state(live).await?;
        ensure!(final_state.1 == before.1 - charge("0.01") - charge("0.02"), "exact financing balance");
        ensure!(final_state.3["SOFR"].len() == 1, "superseded predecessor prunes once no calendar can need it");
        anyhow::Ok(())
    })
    .await;

    let _ = shutdown.send(());
    let publisher_result = tokio::time::timeout(Duration::from_secs(5), &mut publishers).await;
    if publisher_result.is_err() {
        publishers.abort();
        let _ = publishers.await;
    }
    let node_result = if let Some(Node { tx, mut task, api, .. }) = node {
        api.abort();
        let _ = api.await;
        drop(tx);
        let result = tokio::time::timeout(Duration::from_secs(5), &mut task).await;
        if result.is_err() {
            task.abort();
            let _ = task.await;
        }
        Some(result)
    } else {
        None
    };
    scenario.expect("calendar retention scenario finishes within its deadline").expect("calendar retention invariant");
    publisher_result.expect("publisher shuts down").unwrap().unwrap();
    node_result.expect("node remains available for cleanup").expect("sequencer drains").unwrap().unwrap();

    let entries = log_entries(&dir);
    let mut financing = Vec::new();
    let mut observations = Vec::new();
    let mut failed_snapshots = Vec::new();
    let mut parameter_changes = 0;
    for entry in &entries {
        let LogEntry::Command(command) = entry else { continue };
        let events = command.events.as_ref().unwrap();
        financing.extend(charges(events));
        for event in events {
            if let Event::BenchmarkRateObserved { benchmark, effective_date, rollover_date, .. } = event
                && benchmark == "SOFR"
            {
                observations.push((*effective_date, *rollover_date));
            }
        }
        if let Command::SignedAdmin(request) = &command.sequenced.cmd {
            match &request.command {
                AdminCommand::SetBenchmarkRates(batch) if batch.rates.is_empty() => {
                    failed_snapshots.push(batch.as_of);
                    assert!(batch.checked.contains("SOFR") && batch.checked.contains("ESTR"));
                    assert!(events.contains(&Event::AdminApplied));
                }
                AdminCommand::SetParams(_) => {
                    parameter_changes += 1;
                    assert!(events.contains(&Event::AdminApplied));
                }
                _ => {}
            }
        }
    }
    assert_eq!(
        failed_snapshots,
        ["2026-10-19".parse().unwrap(), "2026-10-20".parse().unwrap()],
        "each changed calendar gets a complete attempt; refused attempts stay out of the public log"
    );
    assert_eq!(parameter_changes, 2, "both legal calendar changes use signed HTTPS admin commands");
    assert_eq!(financing, [charge("0.01"), charge("0.02")]);
    let older = "2026-10-19".parse().unwrap();
    let later = "2026-10-20".parse().unwrap();
    assert_eq!(observations, [(older, older), (later, later)]);
    let replayed = replay(&entries).unwrap();
    assert_eq!(replayed.engine.accounts()[&TRADER].balance, fixed("100000") - financing.into_iter().sum::<Fixed>());
    assert_eq!(replayed.engine.benchmark_history()["SOFR"].len(), 1);
    assert_eq!(replayed.engine.benchmark_history()["SOFR"][&later], fixed("0.02"));
}

async fn run_admin(node: &Node, config: &Path, action: &[&str], clock: fn() -> Millis) -> anyhow::Result<AdminRequest> {
    let mut args =
        vec!["engine", "--config", config.to_str().context("config path")?, "--node", &node.url, "--pin", &node.pin];
    args.extend_from_slice(action);
    admin::run_with_clock(
        Cli::try_parse_from(args)?.args,
        KeyEnv { key_file: None, key: Some(HexKey("42".repeat(32))) },
        clock,
    )
    .await
}

async fn retention_state(node: &Node) -> anyhow::Result<(Option<Millis>, Fixed, Option<u64>, BenchmarkHistory)> {
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
    response.await.context("retention state query")
}
