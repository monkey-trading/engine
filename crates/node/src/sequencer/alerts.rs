//! The alerts the sequencer raises: for engine events an operator must look at, and for a house in
//! deficit.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::command::Command;
use engine_core::deposits::{DepositStatus, DepositUpdate};
use engine_core::event::Event;
use engine_core::{Engine, Fixed};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::throttle::{REALERT_AFTER, Throttle};

/// Whether house NAV was below zero after the last command, the throttle on the alert for it, and
/// the alerted deficit not yet resolved, with since when NAV has been back at or above zero.
#[derive(Default)]
pub(super) struct DeficitAlert {
    in_deficit: bool,
    throttle: Throttle,
    alerted: bool,
    recovered_since: Option<Instant>,
}

/// Conditions last emitted for benchmarks used by the active catalogue.
#[derive(Default)]
pub(super) struct BenchmarkAlerts {
    tracked: BTreeMap<String, BenchmarkConditions>,
}

/// Unknown after restart, firing when false, and resolved when true.
#[derive(Default)]
struct BenchmarkConditions {
    missing: Option<bool>,
    undated: Option<bool>,
    stale: Option<bool>,
}

impl DeficitAlert {
    /// Alerts when house NAV `nav` falls below zero, at most once per `REALERT_AFTER` while it keeps
    /// crossing zero, as a house that starts with no capital does with every price. Trading carries
    /// on in a deficit, but client balances then exceed what the house can cover, which only the
    /// operator can repair by adding capital. Resolves the alert once NAV has stayed at or above
    /// zero for `REALERT_AFTER`, so a house crossing zero with every price does not churn pages.
    pub(super) fn note(&mut self, nav: Fixed) {
        let now = Instant::now();
        if !nav.is_negative() {
            if self.in_deficit {
                self.in_deficit = false;
                self.recovered_since = Some(now);
                debug!(%nav, "house NAV back at or above zero");
            }
            if self.alerted && self.recovered_since.is_some_and(|since| now.duration_since(since) >= REALERT_AFTER) {
                info!(alert_resolved = "house_deficit", "house NAV has stayed at or above zero: {nav}");
                self.alerted = false;
            }
            return;
        }
        self.recovered_since = None;
        if self.in_deficit {
            return;
        }
        self.in_deficit = true;
        if let Some(suppressed) = self.throttle.ready(now) {
            self.alerted = true;
            // The reading and count change with each crossing, so they go in the message, not the episode key.
            warn!(
                alert = "house_deficit",
                "house NAV below zero at {nav} ({suppressed} more since the last alert); trading continues within \
                 the cap floors"
            );
        }
    }
}

impl BenchmarkAlerts {
    /// Emits firing conditions repeatedly, and each clear once after startup or firing.
    pub(super) fn note(&mut self, event: &Event) {
        match event {
            // The day goes uncharged, and nothing charges it later.
            Event::BenchmarkRateMissing { benchmark, markets } => {
                self.tracked.entry(benchmark.clone()).or_default().missing = Some(false);
                error!(
                    alert = "benchmark_rate_missing",
                    benchmark, "no benchmark rate for markets {markets:?}; no financing charged"
                );
            }
            Event::BenchmarkRateUndated { benchmark } => {
                let conditions = self.tracked.entry(benchmark.clone()).or_default();
                resolve(
                    &mut conditions.missing,
                    "benchmark_rate_missing",
                    benchmark,
                    "benchmark has an effective rate",
                );
                conditions.undated = Some(false);
                error!(
                    alert = "benchmark_rate_undated",
                    benchmark, "legacy benchmark has no publisher date; financing still applies"
                );
            }
            Event::BenchmarkRateObserved { benchmark, effective_date, rollover_date, stale } => {
                let conditions = self.tracked.entry(benchmark.clone()).or_default();
                resolve(&mut conditions.undated, "benchmark_rate_undated", benchmark, "benchmark has a publisher date");
                resolve(
                    &mut conditions.missing,
                    "benchmark_rate_missing",
                    benchmark,
                    "benchmark has an effective rate",
                );
                if *stale {
                    conditions.stale = Some(false);
                    error!(
                        alert = "benchmark_rate_stale",
                        benchmark,
                        "rate effective {effective_date} is older than five business days at {rollover_date}; financing still applies"
                    );
                } else {
                    resolve(&mut conditions.stale, "benchmark_rate_stale", benchmark, "benchmark is current");
                }
            }
            _ => {}
        }
    }

    /// Benchmarks used by at least one current market.
    pub(super) fn listed(engine: &Engine) -> BTreeSet<String> {
        engine.markets().values().map(|market| market.spec.financing.benchmark.clone()).collect()
    }

    /// Resolves retired names after all accepted-command events, including unknown restart state.
    pub(super) fn retain_listed(&mut self, engine: &Engine, previous: &BTreeSet<String>) {
        let listed = Self::listed(engine);
        for benchmark in previous.difference(&listed) {
            let conditions = self.tracked.entry(benchmark.clone()).or_default();
            for (condition, name) in [
                (&mut conditions.missing, "benchmark_rate_missing"),
                (&mut conditions.undated, "benchmark_rate_undated"),
                (&mut conditions.stale, "benchmark_rate_stale"),
            ] {
                resolve(condition, name, benchmark, "benchmark is no longer used by a listed market");
            }
        }
        self.tracked.retain(|benchmark, _| listed.contains(benchmark.as_str()));
    }
}

#[cfg(test)]
mod retirement_tests;

#[cfg(test)]
mod retirement_webhook_tests;

#[cfg(test)]
mod undated_tests;

/// Emits the first unknown clear and subsequent clears only after the condition fired again.
fn resolve(condition: &mut Option<bool>, name: &str, benchmark: &str, message: &str) {
    if condition.replace(true) != Some(true) {
        info!(alert_resolved = name, benchmark, "{message}");
    }
}

/// The reference of the receipt a command reinstated after its reversal, if it did: an exact
/// reappearance, or a replacement that returns a reversed receipt to the chain.
pub(super) fn reinstated_receipt(cmd: &Command, events: &[Event]) -> Option<String> {
    let Command::CustodyDeposit(update) = cmd else { return None };
    let receipt = match update.as_ref() {
        DepositUpdate::Reinstate { receipt, .. } => receipt,
        DepositUpdate::Replace { replacement, replacement_reversed_at_seq: Some(_), .. } => replacement,
        _ => return None,
    };
    events
        .iter()
        .any(|event| {
            matches!(event, Event::CustodyDepositChanged { receipt: changed, status: DepositStatus::Unfinalised, .. }
                if changed == receipt)
        })
        .then(|| receipt.reference())
}

/// Ends the reversal episode of a reinstated receipt; no other transition can have raised it.
pub(super) fn resolve_reversal(chain_ref: &str) {
    info!(alert_resolved = "deposit_reversed", chain_ref, "reversed receipt reinstated on the canonical chain");
}

/// Raises an alert for an engine event an operator must look at.
pub(super) fn alert_on(event: &Event) {
    match event {
        // A finalised deposit the engine will not credit leaves user funds in the vault and
        // nothing re-examines it.
        Event::DepositRejected { account, amount, chain_ref, reason } => {
            error!(alert = "deposit_rejected", %account, %amount, chain_ref, ?reason, "deposit not credited");
        }
        Event::CustodyDepositChanged { receipt, status: engine_core::deposits::DepositStatus::Reversed, .. } => {
            error!(alert = "deposit_reversed", chain_ref = %receipt.reference(), "quorum proved a credited deposit reversed");
        }
        Event::CustodyDepositRejected { receipt, reason } => {
            error!(alert = "custody_deposit_rejected", chain_ref = %receipt.reference(), ?reason, "custody transition refused");
        }
        Event::PricesRejected { market, reason } => {
            // The reason can carry a per-quote value, so it goes in the message, not the episode key.
            warn!(alert = "prices_rejected", market, "price update refused: {reason}");
        }
        Event::BadDebt { account, amount } => {
            warn!(alert = "bad_debt", %account, %amount, "loss beyond balance written off against the house");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trusted_time::tests::logs::Logs;

    #[test]
    fn a_benchmark_with_no_rate_raises_a_critical_alert() {
        let (logs, _guard) = Logs::capture();
        BenchmarkAlerts::default().note(&Event::BenchmarkRateMissing { benchmark: "Sofr".into(), markets: vec![11] });
        assert_eq!(logs.critical("benchmark_rate_missing"), 1);
        assert_eq!(logs.count("benchmark=\"Sofr\""), 1);
    }
}

#[cfg(test)]
mod benchmark_tests {
    use super::*;

    use std::sync::Arc;

    use engine_core::auth::ClientAuth;
    use engine_core::command::{AdminCommand, Command};
    use engine_core::protocol::ProtocolVersion;
    use tokio::sync::{broadcast, mpsc, watch};

    use crate::log::Manifest;
    use crate::sequencer::Sequencer;
    use crate::tee::EnclaveKey;
    use crate::test_support::{dev_config, scratch_dir, sequencer_config};
    use crate::trusted_time::tests::logs::Logs;

    #[test]
    fn stale_and_undated_rates_raise_critical_alerts_and_current_rates_resolve_them() {
        let (logs, _guard) = Logs::capture();
        let mut alerts = BenchmarkAlerts::default();
        alerts.note(&Event::BenchmarkRateMissing { benchmark: "SOFR".into(), markets: vec![1] });
        alerts.note(&Event::BenchmarkRateUndated { benchmark: "SOFR".into() });
        for _ in 0..3 {
            alerts.note(&observed("SOFR", true));
        }
        assert_eq!(logs.critical("benchmark_rate_missing"), 1);
        assert_eq!(logs.critical("benchmark_rate_undated"), 1);
        assert_eq!(logs.critical("benchmark_rate_stale"), 3);
        assert_eq!(logs.count("alert_resolved=\"benchmark_rate_undated\""), 1);
        assert_eq!(logs.count("alert_resolved=\"benchmark_rate_missing\""), 1);
        alerts.note(&observed("SOFR", false));
        alerts.note(&observed("SOFR", false));
        assert_eq!(logs.critical("benchmark_rate_stale"), 3);
        assert_eq!(logs.count("alert_resolved=\"benchmark_rate_stale\""), 1);
        assert_eq!(logs.count("alert_resolved=\"benchmark_rate_undated\""), 1);
        assert_eq!(logs.count("alert_resolved=\"benchmark_rate_missing\""), 1);
        for _ in 0..2 {
            alerts.note(&observed("SOFR", true));
        }
        alerts.note(&observed("SOFR", false));
        assert_eq!(logs.critical("benchmark_rate_stale"), 5);
        assert_eq!(logs.count("alert_resolved=\"benchmark_rate_stale\""), 2);
    }

    #[test]
    fn repeated_healthy_benchmark_observations_resolve_each_condition_once() {
        let (logs, _guard) = Logs::capture();
        let mut alerts = BenchmarkAlerts::default();
        for benchmark in ["SOFR", "SONIA"] {
            for _ in 0..100 {
                alerts.note(&observed(benchmark, false));
            }
            assert_eq!(logs.count(&format!("benchmark=\"{benchmark}\"")), 3);
        }
        for condition in ["missing", "undated", "stale"] {
            assert_eq!(logs.count(&format!("alert_resolved=\"benchmark_rate_{condition}\"")), 2);
            assert_eq!(logs.critical(&format!("benchmark_rate_{condition}")), 0);
        }
        assert_eq!(alerts.tracked.len(), 2);
    }

    #[test]
    fn missing_and_undated_benchmark_firings_reopen_their_resolution() {
        let (logs, _guard) = Logs::capture();
        let mut alerts = BenchmarkAlerts::default();
        alerts.note(&observed("SOFR", false));
        for (event, name) in [
            (Event::BenchmarkRateMissing { benchmark: "SOFR".into(), markets: vec![1] }, "benchmark_rate_missing"),
            (Event::BenchmarkRateUndated { benchmark: "SOFR".into() }, "benchmark_rate_undated"),
        ] {
            for _ in 0..2 {
                alerts.note(&event);
                alerts.note(&event);
                alerts.note(&observed("SOFR", false));
                alerts.note(&observed("SOFR", false));
            }
            assert_eq!(logs.critical(name), 4);
            assert_eq!(logs.count(&format!("alert_resolved=\"{name}\"")), 3);
        }
        assert_eq!(logs.count("alert_resolved=\"benchmark_rate_stale\""), 1);
    }

    #[test]
    fn a_fresh_benchmark_tracker_resolves_unknown_conditions_once_again() {
        let (logs, _guard) = Logs::capture();
        for _ in 0..2 {
            let mut alerts = BenchmarkAlerts::default();
            for _ in 0..10 {
                alerts.note(&observed("SOFR", false));
            }
        }
        for condition in ["missing", "undated", "stale"] {
            assert_eq!(logs.count(&format!("alert_resolved=\"benchmark_rate_{condition}\"")), 2);
        }
    }

    #[test]
    fn accepted_market_changes_prune_benchmark_alerts_after_their_rollover_events() {
        const START: i64 = 1_792_504_800_000;
        let dir = scratch_dir("benchmark-alert-pruning");
        let (stream, _) = broadcast::channel(16);
        let (manifest, _) = watch::channel(Manifest::default());
        let (withdrawals, _) = mpsc::unbounded_channel();
        let mut seq =
            Sequencer::recover(sequencer_config(&dir, manifest), Arc::new(EnclaveKey::generate()), stream, withdrawals)
                .unwrap();
        let genesis = dev_config().genesis;
        let mut market = genesis.markets[0].clone();
        assert_eq!(market.financing.benchmark, "SOFR");
        for command in [
            AdminCommand::SetClientAuth(ClientAuth::Unsigned),
            AdminCommand::SetOperator(seq.operator),
            AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST },
            AdminCommand::SetParams(genesis.params),
            AdminCommand::UpsertMarket(market.clone()),
        ] {
            let events = seq.stamp(START, Command::Admin(command)).unwrap();
            assert!(events.contains(&Event::AdminApplied), "{events:?}");
        }
        let first = seq.engine.next_rollover().unwrap();
        seq.stamp(first + 1, Command::Tick).unwrap();
        assert_eq!(seq.benchmark_alerts.tracked["SOFR"].missing, Some(false));

        market.financing.benchmark = "SONIA".into();
        let second = seq.engine.next_rollover().unwrap();
        let events = seq.stamp(second + 1, Command::Admin(AdminCommand::UpsertMarket(market.clone()))).unwrap();
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::BenchmarkRateMissing { benchmark, .. } if benchmark == "SOFR"))
        );
        assert_eq!(seq.engine.markets()[&market.id].spec.financing.benchmark, "SONIA");
        assert!(seq.benchmark_alerts.tracked.is_empty(), "retired names must not be reinserted by old rollover events");

        let third = seq.engine.next_rollover().unwrap();
        seq.stamp(third + 1, Command::Tick).unwrap();
        assert_eq!(seq.benchmark_alerts.tracked.len(), 1);
        assert_eq!(seq.benchmark_alerts.tracked["SONIA"].missing, Some(false));
        market.financing.benchmark = "SOFR".into();
        let events = seq.stamp(third + 1, Command::Admin(AdminCommand::UpsertMarket(market))).unwrap();
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        assert!(seq.benchmark_alerts.tracked.is_empty());
        let fourth = seq.engine.next_rollover().unwrap();
        seq.stamp(fourth + 1, Command::Tick).unwrap();
        let returned = &seq.benchmark_alerts.tracked["SOFR"];
        assert_eq!(seq.benchmark_alerts.tracked.len(), 1);
        assert_eq!((returned.missing, returned.undated, returned.stale), (Some(false), None, None));
    }

    /// A current or stale observation for one rollover-local date.
    fn observed(benchmark: &str, stale: bool) -> Event {
        Event::BenchmarkRateObserved {
            benchmark: benchmark.into(),
            effective_date: if stale { "2026-10-20" } else { "2026-10-28" }.parse().unwrap(),
            rollover_date: "2026-10-28".parse().unwrap(),
            stale,
        }
    }
}
