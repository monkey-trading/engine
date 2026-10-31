//! Retired benchmark incidents resolve through the production sequencer and durable webhook route.

use super::*;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use engine_core::auth::ClientAuth;
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
use engine_core::command::{AdminCommand, Command};
use engine_core::protocol::ProtocolVersion;
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
use crate::log::Manifest;
use crate::metrics::{Metrics, SeverityLabel};
use crate::sequencer::fixture_tests::replay_trusted as replay;
use crate::sequencer::{Sequencer, SequencerConfig};
use crate::tee::EnclaveKey;
use crate::test_support::{
    dev_config, keep_callsites_interested, log_entries, sample_around, scratch_dir, sequencer_config,
};

const START: i64 = 1_792_504_800_000;

#[tokio::test]
async fn retiring_benchmarks_resolve_critical_webhook_episodes_after_command_rollovers() {
    keep_callsites_interested();
    for condition in ["missing", "undated", "stale"] {
        for fired_earlier in [false, true] {
            let dir = scratch_dir(&format!("benchmark-retirement-webhook-{condition}-{fired_earlier}"));
            let log_dir = dir.join("log");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/hook", listener.local_addr().unwrap());
            let mut cfg = dev_config().alerts;
            cfg.critical = Some(WebhookRoute { url_env: "RETIREMENT_HOOK".into() });
            cfg.warning = None;
            cfg.outbox.dir = dir.join("outbox");
            cfg.outbox.max_pages = 32;
            cfg.outbox.max_bytes = 262_144;
            cfg.outbox.validate_location(&log_dir).unwrap();
            let routes = Routes::from_config(&cfg, |name| (name == "RETIREMENT_HOOK").then(|| url.clone())).unwrap();
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
            let scenario = with_default(subscriber, || retire_with_rollover(&log_dir, condition, fired_earlier));
            routing.drain(Duration::from_secs(10)).await;
            let _ = shutdown.send(());
            let receiver_result = timeout(Duration::from_secs(5), &mut receiver).await;
            if receiver_result.is_err() {
                receiver.abort();
                let _ = receiver.await;
            }

            let (firing_seq, retiring_seq) = scenario.expect("accepted retirement and bounded catalogue tracker");
            receiver_result.expect("webhook receiver shuts down").unwrap().unwrap();
            let alert = format!("benchmark_rate_{condition}");
            let received: Vec<Page> = std::iter::from_fn(|| pages_rx.try_recv().ok()).collect();
            let episode: Vec<_> = received
                .iter()
                .filter(|page| page.alert == alert && page.fields.get("benchmark").map(String::as_str) == Some("SOFR"))
                .collect();
            assert_eq!(episode.len(), 2, "one firing and one resolution: {condition}, earlier={fired_earlier}");
            assert_eq!((episode[0].status, episode[1].status), (Status::Firing, Status::Resolved));
            let key = format!("{alert}{{benchmark=SOFR}}");
            for page in &episode {
                assert_eq!(page.severity, Severity::Critical);
                assert_eq!(page.key, key);
                assert_eq!(page.fields.len(), 1);
                assert_eq!(page.fields.get("benchmark").map(String::as_str), Some("SOFR"));
            }
            assert_eq!(episode[0].context, format!("command{{id={firing_seq}}}"));
            assert_eq!(episode[1].context, format!("command{{id={retiring_seq}}}"));
            assert!(cfg.outbox.dir.join("outbox.sqlite").is_file(), "pages use the durable outbox");
            assert_eq!(metrics.alerts_dropped.get(), 0);
            assert_eq!(metrics.alert_outbox_overflows.get(), 0);
            assert_eq!(metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity: "critical" }).get(), 0);
        }
    }
}

/// Receives actual JSON pages without blocking the delivery task or sharing mutable test state.
async fn receive_page(State(pages): State<mpsc::Sender<Page>>, Json(page): Json<Page>) -> StatusCode {
    if pages.try_send(page).is_ok() { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR }
}

/// Retires the only old-name market after its rollover emits the condition being tested.
/// Error cases:
/// - sequencer setup or logging fails, an admin command rejects, or the retirement invariant fails
fn retire_with_rollover(log_dir: &Path, condition: &str, fired_earlier: bool) -> anyhow::Result<(u64, u64)> {
    let (stream, _) = broadcast::channel(16);
    let (manifest, _) = watch::channel(Manifest::default());
    let (withdrawals, _) = mpsc::unbounded_channel();
    let mut seq =
        Sequencer::recover(sequencer_config(log_dir, manifest), Arc::new(EnclaveKey::test_key()), stream, withdrawals)?;
    let genesis = dev_config().genesis;
    let mut market = genesis.markets[0].clone();
    ensure!(market.financing.benchmark == "SOFR", "fixture's sole benchmark");
    for command in [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(seq.operator),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V17 },
        AdminCommand::SetParams(genesis.params),
        AdminCommand::UpsertMarket(market.clone()),
    ] {
        let events = seq.stamp(START, Command::Admin(command))?;
        ensure!(events.contains(&Event::AdminApplied), "fixture command rejected: {events:?}");
    }
    if condition == "undated" {
        let events = seq.stamp(
            START,
            Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 100 }),
        )?;
        ensure!(events.contains(&Event::AdminApplied), "legacy rate setup: {events:?}");
    }
    let activated =
        seq.stamp(START, Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 }))?;
    ensure!(activated.contains(&Event::AdminApplied), "V18 activation: {activated:?}");
    let mut rates = vec![BenchmarkRate {
        benchmark: "SONIA".into(),
        effective_date: "2026-10-20".parse()?,
        annual_rate: "0.01".parse()?,
    }];
    if condition == "stale" {
        rates.push(BenchmarkRate {
            benchmark: "SOFR".into(),
            effective_date: "2026-10-01".parse()?,
            annual_rate: "0.01".parse()?,
        });
    }
    let batch =
        BenchmarkBatch { as_of: "2026-10-20".parse()?, checked: ["SOFR".to_owned(), "SONIA".to_owned()].into(), rates };
    let events = seq.stamp(START, Command::Admin(AdminCommand::SetBenchmarkRates(batch)))?;
    ensure!(events.contains(&Event::AdminApplied), "dated replacement rate setup: {events:?}");
    let first_seq = seq.engine.last_seq().context("fixture commands sequenced")? + 1;
    if fired_earlier {
        let first = seq.engine.next_rollover().context("first rollover")?;
        let events = seq.stamp(first + 1, Command::Tick)?;
        ensure!(has_condition(&events, condition), "prior rollover must fire {condition}: {events:?}");
        ensure!(seq.benchmark_alerts.tracked.len() == 1, "only the active benchmark is tracked");
    }
    let retiring_seq = seq.engine.last_seq().context("before retirement")? + 1;
    let retiring_at = seq.engine.next_rollover().context("retiring command's rollover")? + 1;
    market.financing.benchmark = "SONIA".into();
    let events = seq.stamp(retiring_at, Command::Admin(AdminCommand::UpsertMarket(market.clone())))?;
    ensure!(events.contains(&Event::AdminApplied), "retiring market update applies: {events:?}");
    ensure!(has_condition(&events, condition), "retiring command emits the old-name rollover: {events:?}");
    ensure!(seq.engine.markets()[&market.id].spec.financing.benchmark == "SONIA", "last old-name use retired");
    ensure!(seq.benchmark_alerts.tracked.is_empty(), "old rollover events cannot reinsert the retired name");

    // Repeating the accepted update cannot repeat an already resolved retired episode.
    let repeated = seq.stamp(retiring_at, Command::Admin(AdminCommand::UpsertMarket(market)))?;
    ensure!(repeated.contains(&Event::AdminApplied), "repeated catalogue update applies");
    let next = seq.engine.next_rollover().context("new benchmark's rollover")?;
    seq.stamp(next + 1, Command::Tick)?;
    ensure!(seq.benchmark_alerts.tracked.len() == 1, "tracker is bounded by the active catalogue");
    ensure!(seq.benchmark_alerts.tracked.contains_key("SONIA"), "only the replacement benchmark remains");
    seq.flush()?;
    Ok((if fired_earlier { first_seq } else { retiring_seq }, retiring_seq))
}

/// Identifies the real engine event that fires the old benchmark's incident.
fn has_condition(events: &[Event], condition: &str) -> bool {
    events.iter().any(|event| match (condition, event) {
        ("missing", Event::BenchmarkRateMissing { benchmark, .. }) => benchmark == "SOFR",
        ("undated", Event::BenchmarkRateUndated { benchmark }) => benchmark == "SOFR",
        ("stale", Event::BenchmarkRateObserved { benchmark, stale: true, .. }) => benchmark == "SOFR",
        _ => false,
    })
}

#[tokio::test]
async fn retirement_after_restart_resolves_delivered_critical_incidents_without_another_rollover() {
    keep_callsites_interested();
    for condition in ["missing", "undated", "stale"] {
        let dir = scratch_dir(&format!("benchmark-retirement-restart-webhook-{condition}"));
        let log_dir = dir.join("log");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let mut cfg = dev_config().alerts;
        cfg.critical = Some(WebhookRoute { url_env: "RETIREMENT_RESTART_HOOK".into() });
        cfg.warning = None;
        cfg.outbox.dir = dir.join("outbox");
        cfg.outbox.max_pages = 32;
        cfg.outbox.max_bytes = 262_144;
        cfg.outbox.validate_location(&log_dir).unwrap();
        let metrics = Arc::new(Metrics::new());
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
        let scenario = async {
            let routes = Routes::from_config(&cfg, |_| Some(url.clone()))?;
            let (layer, router) = channel(routes, metrics.clone());
            let routing = router.spawn(Duration::from_secs(3600), &cfg.outbox).await?;
            let subscriber = registry().with(layer.with_filter(filter_fn(is_alert_callsite)));
            let written = with_default(subscriber, || firing_log_before_restart(&log_dir, condition));
            routing.drain(Duration::from_secs(10)).await;
            let (firing_seq, next_rollover) = written?;
            let alert = format!("benchmark_rate_{condition}");
            let before: Vec<Page> = std::iter::from_fn(|| pages_rx.try_recv().ok()).collect();
            let mut firing: Vec<_> = before.into_iter().filter(|page| page.alert == alert).collect();
            ensure!(firing.len() == 1, "one old incident was delivered before restarting: {firing:?}");
            let firing = firing.pop().context("old firing page")?;
            ensure!(firing.status == Status::Firing, "external incident is still firing before restart");
            ensure!(firing.severity == Severity::Critical, "old firing severity");
            ensure!(firing.key == format!("{alert}{{benchmark=SOFR}}"), "old external episode key");
            ensure!(
                firing.fields.len() == 1 && firing.fields.get("benchmark").map(String::as_str) == Some("SOFR"),
                "old external subject fields"
            );
            ensure!(firing.context == format!("command{{id={firing_seq}}}"), "old firing command context");
            ensure!(cfg.outbox.dir.join("outbox.sqlite").is_file(), "first router used the durable outbox");
            ensure!(
                metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity: "critical" }).get() == 0,
                "delivered firing was acknowledged before reopening the outbox"
            );

            // Both local trackers restart, while the previously delivered external incident persists.
            let routes = Routes::from_config(&cfg, |_| Some(url.clone()))?;
            let (layer, router) = channel(routes, metrics.clone());
            let routing = router.spawn(Duration::from_secs(3600), &cfg.outbox).await?;
            let subscriber = registry().with(layer.with_filter(filter_fn(is_alert_callsite)));
            let retired = with_default(subscriber, || retirement_before_next_rollover(&log_dir, next_rollover));
            routing.drain(Duration::from_secs(10)).await;
            let (sync_seq, retiring_seq) = retired?;
            let after: Vec<Page> = std::iter::from_fn(|| pages_rx.try_recv().ok()).collect();
            let mut resolved: Vec<_> = after.into_iter().filter(|page| page.alert == alert).collect();
            ensure!(resolved.len() == 1, "only retirement resolves the old incident: {resolved:?}");
            let resolved = resolved.pop().context("post-restart resolution")?;
            ensure!(resolved.status == Status::Resolved, "retirement resolves despite an unknown local condition");
            ensure!(resolved.severity == firing.severity, "resolution preserves Critical severity");
            ensure!(resolved.key == firing.key, "resolution matches the external episode key");
            ensure!(resolved.fields == firing.fields, "resolution preserves the external subject fields");
            ensure!(resolved.context == format!("command{{id={retiring_seq}}}"), "retiring command resolves");
            ensure!(
                resolved.context != format!("command{{id={sync_seq}}}"),
                "unchanged startup catalogue cannot resolve"
            );
            let replayed = replay(&log_entries(&log_dir))?;
            replayed.genesis.ensure_closed()?;
            ensure!(
                replayed.engine.next_rollover() == Some(next_rollover),
                "restart and retirement charge no rollover"
            );
            let required = replayed.engine.benchmark_state().required;
            ensure!(required.len() == 1 && required.contains("SONIA"), "retirement replays");
            anyhow::Ok(())
        }
        .await;
        let _ = shutdown.send(());
        let receiver_result = timeout(Duration::from_secs(5), &mut receiver).await;
        if receiver_result.is_err() {
            receiver.abort();
            let _ = receiver.await;
        }

        scenario.expect("restart retirement resolves the externally firing incident");
        receiver_result.expect("webhook receiver shuts down").unwrap().unwrap();
        assert_eq!(metrics.alerts_dropped.get(), 0);
        assert_eq!(metrics.alert_outbox_overflows.get(), 0);
        assert_eq!(metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity: "critical" }).get(), 0);
    }
}

/// Writes a complete genesis, fires one benchmark incident, checkpoints, and closes the public log.
/// Error cases:
/// - recovery, genesis, command validation, or durable log writes fail
fn firing_log_before_restart(log_dir: &Path, condition: &str) -> anyhow::Result<(u64, i64)> {
    let mut seq = retirement_recover_at(log_dir, START)?;
    if condition == "undated" {
        // A real V17 genesis supplies the legacy scalar without inventing a publisher date.
        for command in [
            AdminCommand::SetClientAuth(ClientAuth::Unsigned),
            AdminCommand::SetOperator(seq.operator),
            AdminCommand::ActivateProtocol { version: ProtocolVersion::V17 },
        ] {
            let events = seq.stamp(START, Command::Admin(command))?;
            ensure!(events.contains(&Event::AdminApplied), "legacy activation applies: {events:?}");
        }
    } else {
        seq.activate_protocol()?;
    }
    let mut genesis = dev_config().genesis;
    genesis.markets.retain(|market| market.id == 1);
    genesis.close_spread_ceilings.retain(|ceiling| ceiling.market == 1);
    ensure!(genesis.markets.len() == 1 && genesis.markets[0].financing.benchmark == "SOFR", "only SOFR market");
    genesis.benchmarks.retain(|benchmark| condition == "undated" && benchmark.name == "SOFR");
    seq.genesis(&genesis)?;
    seq.activate_protocol()?;
    let mut rates = vec![BenchmarkRate {
        benchmark: "SONIA".into(),
        effective_date: "2026-10-20".parse()?,
        annual_rate: "0.01".parse()?,
    }];
    if condition == "stale" {
        rates.push(BenchmarkRate {
            benchmark: "SOFR".into(),
            effective_date: "2026-10-01".parse()?,
            annual_rate: "0.01".parse()?,
        });
    }
    let batch =
        BenchmarkBatch { as_of: "2026-10-20".parse()?, checked: ["SOFR".to_owned(), "SONIA".to_owned()].into(), rates };
    let cmd = retirement_signed(&seq, AdminCommand::SetBenchmarkRates(batch), START);
    let events = seq.stamp(START, cmd)?;
    ensure!(events.contains(&Event::AdminApplied), "dated replacement history installs");
    let firing_seq = seq.engine.last_seq().context("before firing")? + 1;
    let first = seq.engine.next_rollover().context("first rollover")?;
    let events = seq.stamp(first + 1, Command::Tick)?;
    ensure!(has_condition(&events, condition), "first rollover fires {condition}: {events:?}");
    ensure!(seq.benchmark_alerts.tracked.len() == 1, "old benchmark is locally tracked before restart");
    let next = seq.engine.next_rollover().context("next rollover after firing")?;
    seq.checkpoint()?;
    seq.flush()?;
    drop(seq);
    Ok((firing_seq, next))
}

/// Recovers an unknown tracker, completes startup sync without observations, and retires SOFR.
/// Error cases:
/// - recovery, synchronization, retirement, or durable log writes fail
fn retirement_before_next_rollover(log_dir: &Path, next_rollover: i64) -> anyhow::Result<(u64, u64)> {
    let now = next_rollover - 60_000;
    let mut seq = retirement_recover_at(log_dir, now)?;
    ensure!(seq.benchmark_alerts.tracked.is_empty(), "benchmark conditions are unknown after recovery");
    ensure!(seq.engine.next_rollover() == Some(next_rollover), "no unpaid rollover at restart");
    ensure!(seq.engine.client_auth().is_some() && seq.engine.operator().is_some(), "genesis recorded both auth modes");
    ensure!(
        seq.engine.params().is_some_and(|params| params.toxicity.notional.is_some()),
        "complete genesis recorded the legacy startup weighting prerequisite"
    );
    seq.wait_for_benchmarks()?;
    let as_of = seq.engine.benchmark_date(now);
    let annual_rate: Fixed = "0.02".parse()?;
    let snapshot = BenchmarkBatch {
        as_of,
        checked: ["SOFR".to_owned()].into(),
        rates: vec![BenchmarkRate { benchmark: "SOFR".into(), effective_date: as_of, annual_rate }],
    };
    let sync_seq = seq.engine.last_seq().context("before startup snapshot")? + 1;
    let cmd = retirement_signed(&seq, AdminCommand::SetBenchmarkRates(snapshot), now);
    let events = seq.sequence(cmd)?.context("startup snapshot admitted")?;
    ensure!(events.contains(&Event::AdminApplied), "complete authenticated fetch attempt applies");
    ensure!(!events.iter().any(is_benchmark_status), "startup sync has no rollover status observations");
    ensure!(seq.engine.benchmark_history()["SOFR"].get(&as_of) == Some(&annual_rate), "healthy dated rate installs");
    ensure!(seq.benchmark_alerts.tracked.is_empty(), "startup sync leaves old conditions unknown");
    ensure!(!seq.benchmark_sync, "authenticated startup synchronization completes");

    let mut market = seq.engine.markets().values().next().context("only market")?.spec.clone();
    market.financing.benchmark = "SONIA".into();
    let retiring_seq = seq.engine.last_seq().context("before retirement")? + 1;
    let cmd = retirement_signed(&seq, AdminCommand::UpsertMarket(market.clone()), now);
    let events = seq.sequence(cmd)?.context("retirement admitted")?;
    ensure!(events.contains(&Event::AdminApplied), "market retirement applies");
    ensure!(!events.iter().any(is_benchmark_status), "retirement precedes the next rollover");
    ensure!(seq.engine.markets()[&market.id].spec.financing.benchmark == "SONIA", "last SOFR use retires");
    ensure!(seq.benchmark_alerts.tracked.is_empty(), "retired unknown conditions do not grow the tracker");
    ensure!(seq.engine.next_rollover() == Some(next_rollover), "schedule stays unpaid and unchanged");
    seq.checkpoint()?;
    seq.flush()?;
    Ok((sync_seq, retiring_seq))
}

/// Recovers under a private controlled clock and supplies its first authenticated time sample.
/// Error cases:
/// - log recovery or a queued required command fails
fn retirement_recover_at(log_dir: &Path, at: i64) -> anyhow::Result<Sequencer> {
    let clock = Arc::new(TestClock::at(at));
    let (stream, _) = broadcast::channel(16);
    let (manifest, _) = watch::channel(Manifest::default());
    let (withdrawals, _) = mpsc::unbounded_channel();
    let genesis = crate::sequencer::fixture_tests::finished_genesis(&log_entries(log_dir));
    let cfg = SequencerConfig { clock: clock.clone(), genesis, ..sequencer_config(log_dir, manifest) };
    let mut seq = Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, withdrawals)?;
    seq.trust(sample_around(clock.as_ref()))?;
    Ok(seq)
}

/// Signs the next local test command with the published development operator key.
fn retirement_signed(seq: &Sequencer, command: AdminCommand, now: i64) -> Command {
    let key = SigningKey::from_slice(&[0x42; 32]).unwrap();
    let nonce = seq.engine.next_request_nonce(seq.operator.address);
    Command::SignedAdmin(Box::new(sign(&key, &seq.operator, command, nonce, now + 60_000)))
}

/// Whether a command observed any rollover's missing, undated, or dated benchmark condition.
fn is_benchmark_status(event: &Event) -> bool {
    matches!(
        event,
        Event::BenchmarkRateMissing { .. } | Event::BenchmarkRateUndated { .. } | Event::BenchmarkRateObserved { .. }
    )
}
