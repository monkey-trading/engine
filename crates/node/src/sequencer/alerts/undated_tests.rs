//! Scalar fallback resolves missing financing while its publisher date remains unknown.

use super::*;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::fixed::Rounding;
use engine_core::price::SourceQuote;
use engine_core::types::Side;
use k256::ecdsa::SigningKey;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::timeout;
use tracing::subscriber::with_default;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry;

use crate::admin::sign;
use crate::alerts::{Page, Routes, Severity, Status, channel, is_alert_callsite};
use crate::clock::test_clock::TestClock;
use crate::config::WebhookRoute;
use crate::log::{LogEntry, Manifest};
use crate::metrics::{Metrics, SeverityLabel};
use crate::sequencer::fixture_tests::replay_trusted as replay;
use crate::sequencer::{Sequencer, SequencerConfig};
use crate::tee::EnclaveKey;
use crate::test_support::{
    TRADER, deposit, dev_config, keep_callsites_interested, log_entries, sample_around, scratch_dir, sequencer_config,
};
use crate::trusted_time::tests::logs::Logs;

#[test]
fn benchmark_undated_fallback_resolves_unknown_firing_and_cleared_missing_once() {
    for (previous, stale) in [(None, false), (Some(false), false), (Some(true), false), (Some(true), true)] {
        let (logs, _guard) = Logs::capture();
        let mut alerts = BenchmarkAlerts::default();
        match previous {
            None => {}
            Some(false) => alerts.note(&missing()),
            Some(true) => alerts.note(&observed(stale)),
        }
        let prior_clears = clears(&logs, "missing");
        let prior_stale = alerts.tracked.get("SOFR").and_then(|conditions| conditions.stale);
        for _ in 0..100 {
            alerts.note(&undated());
        }
        assert_eq!(clears(&logs, "missing"), prior_clears + usize::from(previous != Some(true)));
        assert_eq!(alerts.tracked["SOFR"].missing, Some(true));
        assert_eq!(alerts.tracked["SOFR"].undated, Some(false));
        assert_eq!(alerts.tracked["SOFR"].stale, prior_stale, "an unknown date proves no stale/current state");
        assert_eq!(logs.critical("benchmark_rate_undated"), 100, "firing repeats remain available for re-pages");

        let undated_clears = clears(&logs, "undated");
        alerts.note(&missing());
        assert_eq!(alerts.tracked["SOFR"].missing, Some(false));
        assert_eq!(clears(&logs, "undated"), undated_clears, "missing cannot claim a publisher date");
        alerts.note(&undated());
        assert_eq!(clears(&logs, "missing"), prior_clears + usize::from(previous != Some(true)) + 1);
        assert_eq!(clears(&logs, "undated"), undated_clears, "fallback keeps its own episode firing");

        alerts.note(&observed(false));
        alerts.note(&observed(false));
        assert_eq!(clears(&logs, "undated"), undated_clears + 1, "only a dated observation closes undated");
        assert_eq!(clears(&logs, "stale"), 1, "healthy observations clear stale exactly once");
        let before_retirement = [clears(&logs, "missing"), clears(&logs, "undated"), clears(&logs, "stale")];
        alerts.retain_listed(&Engine::new(), &["SOFR".to_owned()].into());
        alerts.retain_listed(&Engine::new(), &BTreeSet::new());
        assert!(alerts.tracked.is_empty());
        assert_eq!(
            [clears(&logs, "missing"), clears(&logs, "undated"), clears(&logs, "stale")],
            before_retirement,
            "retirement cannot repeat already cleared conditions"
        );
        alerts.note(&undated());
        assert_eq!(clears(&logs, "missing"), before_retirement[0] + 1, "a returning name has unknown state");
        assert_eq!(alerts.tracked["SOFR"].undated, Some(false));
    }
}

#[tokio::test]
async fn benchmark_undated_signed_calendar_rewind_resolves_the_matching_durable_missing_page() {
    keep_callsites_interested();
    for restart in [false, true] {
        let dir = scratch_dir(&format!("benchmark-undated-webhook-{restart}"));
        let log_dir = dir.join("log");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let mut cfg = dev_config().alerts;
        cfg.critical = Some(WebhookRoute { url_env: "UNDATED_HOOK".into() });
        cfg.warning = None;
        cfg.outbox.dir = dir.join("outbox");
        cfg.outbox.max_pages = 32;
        cfg.outbox.max_bytes = 262_144;
        cfg.outbox.validate_location(&log_dir).unwrap();
        let routes = Routes::from_config(&cfg, |name| (name == "UNDATED_HOOK").then(|| url.clone())).unwrap();
        let metrics = Arc::new(Metrics::new());
        let (layer, router) = channel(routes, metrics.clone());
        let routing = router.spawn(Duration::from_secs(3600), &cfg.outbox).await.unwrap();
        let subscriber = registry().with(layer.with_filter(filter_fn(is_alert_callsite)));
        let (pages_tx, mut pages_rx) = mpsc::channel(32);
        let app = Router::new().route("/hook", post(receive_page)).with_state(pages_tx);
        let (shutdown, stopped) = oneshot::channel();
        let mut receiver = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        let scenario = with_default(subscriber, || signed_calendar_fallback(&log_dir, restart));
        routing.drain(Duration::from_secs(10)).await;
        let _ = shutdown.send(());
        let receiver_result = timeout(Duration::from_secs(5), &mut receiver).await;
        if receiver_result.is_err() {
            receiver.abort();
            let _ = receiver.await;
        }

        let (missing_seq, fallback_seq) = scenario.expect("signed calendar fallback applies financing");
        receiver_result.expect("webhook receiver shuts down").unwrap().unwrap();
        let pages: Vec<Page> = std::iter::from_fn(|| pages_rx.try_recv().ok()).collect();
        let missing: Vec<_> = pages.iter().filter(|page| page.alert == "benchmark_rate_missing").collect();
        assert_eq!(missing.len(), 2, "one firing and one immediate resolution, restart={restart}: {pages:?}");
        assert_eq!((missing[0].status, missing[1].status), (Status::Firing, Status::Resolved));
        assert_eq!(missing[0].context, format!("command{{id={missing_seq}}}"));
        assert_eq!(missing[1].context, format!("command{{id={fallback_seq}}}"));
        for page in &missing {
            assert_eq!(page.key, "benchmark_rate_missing{benchmark=SOFR}");
            assert_eq!(page.severity, Severity::Critical);
            assert_eq!(page.fields.len(), 1);
            assert_eq!(page.fields.get("benchmark").map(String::as_str), Some("SOFR"));
        }
        let undated: Vec<_> = pages.iter().filter(|page| page.alert == "benchmark_rate_undated").collect();
        assert_eq!(undated.len(), 1, "fallback leaves the undated episode firing: {pages:?}");
        assert_eq!(undated[0].status, Status::Firing);
        assert_eq!(undated[0].severity, Severity::Critical);
        assert_eq!(undated[0].key, "benchmark_rate_undated{benchmark=SOFR}");
        assert_eq!(undated[0].fields, missing[0].fields);
        assert_eq!(undated[0].context, format!("command{{id={fallback_seq}}}"));
        assert!(cfg.outbox.dir.join("outbox.sqlite").is_file());
        assert_eq!(metrics.alerts_dropped.get(), 0);
        assert_eq!(metrics.alert_outbox_overflows.get(), 0);
        assert_eq!(metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity: "critical" }).get(), 0);
    }
}

/// Applies a scalar to a rewound local date after a genuinely uncharged missing-rate rollover.
/// Error cases:
/// - setup, authentication, sequencing, financing assertions, or trusted public replay fail
fn signed_calendar_fallback(log_dir: &Path, restart: bool) -> anyhow::Result<(u64, u64)> {
    let start = chrono::DateTime::parse_from_rfc3339("2026-10-21T10:00:00Z")?.timestamp_millis();
    let first = start + 60_000;
    let manual = first + 30_000;
    let fallback = first + 60_000;
    let clock = Arc::new(TestClock::at(start));
    let mut seq = recover(log_dir, clock.clone())?;
    seq.activate_protocol()?;
    let mut genesis = dev_config().genesis;
    genesis.markets.retain(|market| market.id == 1);
    genesis.benchmarks.clear();
    genesis.params.rollover.tz = chrono_tz::Pacific::Kiritimati;
    genesis.params.rollover.hour = 0;
    genesis.params.rollover.minute = 1;
    seq.genesis(&genesis)?;
    ensure!(seq.engine.next_rollover() == Some(first), "Kiritimati local October22 rollover");
    submit(&mut seq, &clock, start, deposit(100_000, "undated-rewind-deposit"))?;
    submit(
        &mut seq,
        &clock,
        start,
        Command::Prices {
            market: 1,
            quotes: ["venue-a", "venue-b"]
                .into_iter()
                .map(|source| SourceQuote { source: source.into(), mid: Fixed::from_int(100_000), ts: start })
                .collect(),
        },
    )?;
    let filled = submit(
        &mut seq,
        &clock,
        start,
        Command::PlaceOrder(OrderRequest {
            account: TRADER,
            client_id: 1,
            market: 1,
            side: Side::Buy,
            qty: "0.1".parse()?,
            kind: OrderKind::Market { quoted: Fixed::from_int(100_000), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        }),
    )?;
    ensure!(filled.iter().any(|event| matches!(event, Event::Fill(_))), "marked position fills");
    let balance = seq.engine.accounts()[&TRADER].balance;
    let missing = submit(&mut seq, &clock, first + 1, Command::Tick)?;
    ensure!(missing.iter().any(|event| matches!(event, Event::BenchmarkRateMissing { .. })), "no rate exists");
    ensure!(!missing.iter().any(|event| matches!(event, Event::Financing { .. })), "missing date is uncharged");
    ensure!(seq.engine.accounts()[&TRADER].balance == balance, "missing date changes no balance");
    let missing_seq = seq.engine.last_seq().context("missing command sequence")?;
    if restart {
        seq.checkpoint()?;
        seq.flush()?;
        drop(seq);
        seq = recover(log_dir, clock.clone())?;
        ensure!(seq.benchmark_alerts.tracked.is_empty(), "actual recovery leaves condition state unknown");
    }
    let rate = signed(&seq, AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 100 }, manual);
    let events = submit(&mut seq, &clock, manual, rate)?;
    ensure!(events.contains(&Event::AdminApplied), "signed manual rate applies");
    ensure!(!events.iter().any(is_status), "installing a rate alone emits no observed status");
    ensure!(
        seq.engine.benchmark_history()["SOFR"].keys().copied().collect::<Vec<_>>()
            == ["2026-10-22".parse::<chrono::NaiveDate>()?],
        "manual rate records the original schedule's future-only date"
    );
    let mut params = seq.engine.params().context("genesis parameters")?.clone();
    params.rollover.tz = "Etc/GMT+12".parse()?;
    params.rollover.hour = 22;
    params.rollover.minute = 2;
    let rewind = signed(&seq, AdminCommand::SetParams(params), manual);
    let events = submit(&mut seq, &clock, manual, rewind)?;
    ensure!(events.contains(&Event::AdminApplied), "signed legal calendar rewind applies");
    ensure!(!events.iter().any(is_status), "calendar edit alone has no rollover observation");
    ensure!(seq.engine.next_rollover() == Some(fallback), "rewound local October20 rollover");
    ensure!(
        seq.engine.benchmark_date(fallback) == "2026-10-20".parse::<chrono::NaiveDate>()?,
        "rewound date precedes the sole dated observation"
    );
    let events = submit(&mut seq, &clock, fallback + 1, Command::Tick)?;
    let fallback_seq = seq.engine.last_seq().context("fallback command sequence")?;
    ensure!(events.iter().any(|event| matches!(event, Event::BenchmarkRateUndated { .. })), "scalar fallback applies");
    let charge = Fixed::from_int(10_000)
        .mul_rounded(Fixed::from_bps(100 + 500), Rounding::Up)
        .div_int_rounded(365, Rounding::Up);
    ensure!(
        events.contains(&Event::Financing { account: TRADER, market: 1, amount: charge }),
        "scalar financing has the exact notional, markup, day basis and rounding"
    );
    ensure!(seq.engine.accounts()[&TRADER].balance == balance - charge, "only the fallback date charges");
    ensure!(seq.benchmark_alerts.tracked["SOFR"].undated == Some(false), "undated remains firing");
    let repeated = submit(&mut seq, &clock, fallback + 1, Command::Tick)?;
    ensure!(!repeated.iter().any(is_status), "same-date ticker cannot duplicate status or financing");
    ensure!(!repeated.iter().any(|event| matches!(event, Event::Financing { .. })), "same date never charges twice");
    seq.flush()?;
    let entries = log_entries(log_dir);
    let amounts: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(command) => command.events.as_deref(),
            _ => None,
        })
        .flatten()
        .filter_map(|event| match event {
            Event::Financing { amount, .. } => Some(*amount),
            _ => None,
        })
        .collect();
    ensure!(amounts == [charge], "public log records the single exact scalar charge");
    let mut replayed = replay(&entries)?;
    ensure!(replayed.engine.accounts()[&TRADER].balance == balance - charge, "trusted replay preserves financing");
    ensure!(replayed.engine.state_root() == seq.engine.state_root(), "all public roots and events replay");
    Ok((missing_seq, fallback_seq))
}

fn recover(log_dir: &Path, clock: Arc<TestClock>) -> anyhow::Result<Sequencer> {
    let genesis = crate::sequencer::fixture_tests::finished_genesis(&log_entries(log_dir));
    let manifest = watch::channel(Manifest::default()).0;
    let cfg = SequencerConfig { clock: clock.clone(), genesis, ..sequencer_config(log_dir, manifest) };
    let mut seq = Sequencer::recover(
        cfg,
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(16).0,
        mpsc::unbounded_channel().0,
    )?;
    seq.trust(sample_around(clock.as_ref()))?;
    Ok(seq)
}

fn submit(seq: &mut Sequencer, clock: &TestClock, at: i64, command: Command) -> anyhow::Result<Vec<Event>> {
    clock.advance(Duration::from_millis(u64::try_from(at - clock.true_ms())?));
    seq.trust(sample_around(clock))?;
    seq.sequence(command)?.context("fresh trusted time admits the command")
}

fn signed(seq: &Sequencer, command: AdminCommand, at: i64) -> Command {
    let key = SigningKey::from_slice(&[0x42; 32]).unwrap();
    let nonce = seq.engine.next_request_nonce(seq.operator.address);
    Command::SignedAdmin(Box::new(sign(&key, &seq.operator, command, nonce, at + 60_000)))
}

async fn receive_page(State(pages): State<mpsc::Sender<Page>>, Json(page): Json<Page>) -> StatusCode {
    if pages.try_send(page).is_ok() { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR }
}

fn missing() -> Event {
    Event::BenchmarkRateMissing { benchmark: "SOFR".into(), markets: vec![1] }
}

fn undated() -> Event {
    Event::BenchmarkRateUndated { benchmark: "SOFR".into() }
}

fn observed(stale: bool) -> Event {
    Event::BenchmarkRateObserved {
        benchmark: "SOFR".into(),
        effective_date: if stale { "2026-10-01" } else { "2026-10-20" }.parse().unwrap(),
        rollover_date: "2026-10-20".parse().unwrap(),
        stale,
    }
}

fn clears(logs: &Logs, condition: &str) -> usize {
    logs.count(&format!("alert_resolved=\"benchmark_rate_{condition}\""))
}

fn is_status(event: &Event) -> bool {
    matches!(
        event,
        Event::BenchmarkRateMissing { .. } | Event::BenchmarkRateUndated { .. } | Event::BenchmarkRateObserved { .. }
    )
}
