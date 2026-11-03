//! The separate publisher worker: it alone holds the operator key and uses the signed admin API.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::auth::Operator;
use engine_core::benchmark::BenchmarkState;
use engine_core::command::{AdminCommand, AdminRequest};
use engine_core::event::Event;
use engine_core::params::Rollover;
use engine_core::types::{MAX_STAMP_MS, Millis};
use hyper::Method;
use k256::ecdsa::SigningKey;
use tokio::sync::watch as channel;
use tokio::time::{Instant, sleep_until};
use tracing::{info, warn};

use crate::admin::input::NonceView;
use crate::admin::{CALL_TIMEOUT, EXPIRES_IN_MS, NodeApi, benchmark_watch, call, sign, submit};
use crate::benchmarks;
use crate::config::NodeConfig;

/// Fetches and signs periodically and before rollover, or once without watch mode.
/// Error cases:
/// - invalid scheduling bounds, missing config, client setup, or one-shot fetch/submission failure
pub(super) async fn run(
    cfg: &NodeConfig,
    node: &NodeApi,
    key: &SigningKey,
    operator: &Operator,
    clock: impl Fn() -> Millis,
    watch: bool,
) -> anyhow::Result<AdminRequest> {
    let settings = cfg.benchmarks.as_ref().context("sync-benchmarks needs [benchmarks] in the config")?;
    let budget = fetch_budget(settings.timeout, settings.sources.len())?;
    checked_horizon(clock(), budget)?;
    let _ = Instant::now().checked_add(settings.timeout).context("publisher timeout cannot be represented")?;
    let client = benchmarks::client()?;
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let initial = if watch {
        let reply = tokio::select! {
            biased;
            stopped = &mut shutdown => {
                stopped?;
                bail!("benchmark worker stopped before its first snapshot");
            }
            reply = benchmark_watch::current(node, None) => reply,
        };
        match reply {
            Ok(schedule) => {
                info!(alert_resolved = "benchmark_schedule_watch_failed", "benchmark schedule watch connected");
                Some(schedule.revision)
            }
            Err(err) => {
                warn!(alert = "benchmark_schedule_watch_failed", "benchmark schedule watch will reconnect: {err:#}");
                None
            }
        }
    } else {
        None
    };
    let (updates, mut schedules) = channel::channel(initial);
    let retry = settings.interval.min(CALL_TIMEOUT);
    let work = async {
        let mut ticks = tokio::time::interval(settings.interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut rollover = None;
        let mut preflight = None;
        let mut last = None;
        loop {
            tokio::select! {
                biased;
                stopped = &mut shutdown, if watch => {
                    stopped?;
                    return last.context("benchmark worker stopped before its first snapshot");
                }
                changed = schedules.changed(), if watch => {
                    changed.context("benchmark schedule watch stopped")?;
                    schedules.borrow_and_update();
                }
                _ = ticks.tick() => {},
                _ = async {
                    if let Some(at) = preflight {
                        sleep_until(at).await;
                    }
                }, if watch && preflight.is_some() => {},
            }
            let attempt = async {
                // A signed schedule change consumes this nonce, fencing off an older fetch.
                let path = format!("/v1/accounts/{}/nonce", operator.address);
                let NonceView { next_nonce } = call(node, Method::GET, &path, None).await?;
                let state: BenchmarkState = call(node, Method::GET, "/v1/benchmarks", None).await?;
                rollover = state.rollover.clone();
                if rollover.is_none() {
                    bail!("node genesis is still pending trusted time");
                }
                let now = clock();
                checked_horizon(now, budget)?;
                let batch = benchmarks::snapshot(settings, &client, &state, state.date_at(now)).await?;
                let expires_at = checked_horizon(clock(), EXPIRES_IN_MS)?;
                let req = sign(key, operator, AdminCommand::SetBenchmarkRates(batch), next_nonce, expires_at);
                let events = submit(node, &req).await?;
                if let Some(reason) = events.iter().find_map(|event| match event {
                    Event::AdminRejected { reason } => Some(reason),
                    _ => None,
                }) {
                    bail!("benchmark snapshot rejected: {reason}");
                }
                info!(
                    alert_resolved = "benchmark_sync_failed",
                    "signed benchmark snapshot sequenced with nonce {next_nonce}"
                );
                anyhow::Ok(req)
            };
            // A POST may already be durable: finish its acknowledgement before the next attempt.
            let attempt = tokio::select! {
                biased;
                stopped = &mut shutdown, if watch => {
                    stopped?;
                    return last.context("benchmark worker stopped before its first snapshot");
                }
                result = attempt => result,
            };
            let failed = attempt.is_err();
            match attempt {
                Ok(request) if !watch => return Ok(request),
                Ok(request) => last = Some(request),
                Err(err) if !watch => return Err(err),
                Err(err) => {
                    warn!(alert = "benchmark_sync_failed", "benchmark worker will retry: {err:#}")
                }
            }
            preflight = rollover
                .as_ref()
                .map(|schedule| {
                    let delay = preflight_delay(schedule, clock(), budget)?;
                    Instant::now().checked_add(delay).context("rollover preflight wait cannot be represented")
                })
                .transpose()?;
            if failed {
                let retry_at =
                    Instant::now().checked_add(retry).context("benchmark retry wait cannot be represented")?;
                preflight = Some(preflight.map_or(retry_at, |at| at.min(retry_at)));
            }
        }
    };
    tokio::select! {
        result = work => result,
        result = benchmark_watch::run(node, initial, retry, updates), if watch => {
            result?;
            bail!("benchmark schedule watch stopped");
        }
    }
}

/// Reserves every sequential publisher timeout and the three bounded calls to the node.
/// Error cases:
/// - publisher count or the complete millisecond budget overflows
fn fetch_budget(timeout: Duration, publishers: usize) -> anyhow::Result<Millis> {
    let publishers = u32::try_from(publishers).context("benchmark publisher count overflows")?;
    let node_calls = CALL_TIMEOUT.checked_mul(3).context("benchmark node-call budget overflows")?;
    let total = timeout
        .checked_mul(publishers)
        .and_then(|downloads| downloads.checked_add(node_calls))
        .context("benchmark fetch budget overflows")?;
    Millis::try_from(total.as_nanos().div_ceil(1_000_000))
        .context("benchmark fetch budget exceeds timestamp arithmetic")
}

/// Adds a request budget without leaving the engine's supported timestamp range.
/// Error cases:
/// - the clock, arithmetic, or resulting deadline is outside the supported range
fn checked_horizon(now: Millis, budget: Millis) -> anyhow::Result<Millis> {
    ensure!((0..=MAX_STAMP_MS).contains(&now), "benchmark worker clock is outside supported timestamps");
    let until = now.checked_add(budget).context("benchmark scheduling time overflows")?;
    ensure!((now..=MAX_STAMP_MS).contains(&until), "benchmark scheduling deadline is outside supported timestamps");
    Ok(until)
}

/// Starts a full-budget attempt before the next rollover whose preflight window has not begun.
/// Error cases:
/// - the budget or next rollover exceeds supported timestamps, or the delay cannot be represented
fn preflight_delay(rollover: &Rollover, now: Millis, budget: Millis) -> anyhow::Result<Duration> {
    let until = checked_horizon(now, budget)?;
    let next = rollover.next_after(until);
    checked_horizon(next, 0)?;
    let millis = next.checked_sub(until).context("benchmark preflight delay overflows")?;
    Ok(Duration::from_millis(u64::try_from(millis).context("benchmark preflight delay is negative")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::DateTime;
    use chrono_tz::Europe::London;

    #[test]
    fn benchmark_fetch_budget_covers_every_publisher_and_node_call() {
        assert_eq!(fetch_budget(Duration::from_secs(20), 5).unwrap(), 130_000);
        assert_eq!(fetch_budget(Duration::from_secs(2), 2).unwrap(), 34_000);
        assert_eq!(fetch_budget(Duration::from_nanos(1), 1).unwrap(), 30_001);
        assert_eq!(fetch_budget(Duration::ZERO, 0).unwrap(), 30_000);
    }

    #[test]
    fn benchmark_preflight_reserves_the_full_budget_before_next_london_rollover() {
        let now = stamp("2026-10-19T23:00:00+01:00");
        let budget = fetch_budget(Duration::from_secs(2), 2).unwrap();
        let delay = preflight_delay(&london(), now, budget).unwrap();
        assert_eq!(delay, Duration::from_secs(23 * 3600 - 34));
        assert_eq!(now + i64::try_from(delay.as_millis()).unwrap() + budget, stamp("2026-10-20T22:00:00+01:00"));
    }

    #[test]
    fn benchmark_preflight_moves_to_next_day_once_current_window_begins() {
        let current = stamp("2026-10-20T22:00:00+01:00");
        let next = stamp("2026-10-21T22:00:00+01:00");
        let budget = fetch_budget(Duration::from_secs(2), 2).unwrap();
        for completed_after in [0, 1, budget] {
            let now = current - budget + completed_after;
            let delay = preflight_delay(&london(), now, budget).unwrap();
            assert_eq!(now + i64::try_from(delay.as_millis()).unwrap(), next - budget);
            assert!(!delay.is_zero());
        }
    }

    #[test]
    fn benchmark_preflight_respects_london_daylight_saving() {
        let budget = fetch_budget(Duration::from_secs(2), 2).unwrap();
        for (current, hours) in [("2026-03-28T22:00:00Z", 23), ("2026-10-24T21:00:00Z", 25)] {
            let now = stamp(current) - budget;
            assert_eq!(preflight_delay(&london(), now, budget).unwrap(), Duration::from_secs(hours * 3600));
        }
    }

    #[test]
    fn benchmark_preflight_refuses_budget_overflow_and_unsupported_calendar_times() {
        assert!(fetch_budget(Duration::MAX, 5).is_err());
        assert!(fetch_budget(Duration::from_secs(u64::MAX / 2), 1).is_err());
        assert!(preflight_delay(&london(), -1, 34_000).is_err());
        assert!(preflight_delay(&london(), 1, Millis::MAX).is_err());
        assert!(preflight_delay(&london(), MAX_STAMP_MS, 1).is_err());
        assert!(preflight_delay(&london(), MAX_STAMP_MS - 3_600_000, 1_000).is_err());
        assert_eq!(checked_horizon(MAX_STAMP_MS, 0).unwrap(), MAX_STAMP_MS);
    }

    /// Converts an explicit fixture date to milliseconds.
    fn stamp(value: &str) -> Millis {
        DateTime::parse_from_rfc3339(value).unwrap().timestamp_millis()
    }

    /// The production rollover schedule.
    fn london() -> Rollover {
        Rollover { tz: London, hour: 22, minute: 0 }
    }
}
