//! The session's `V46` part: the operator's powers over benchmark rates and the stress floor are
//! bounded for an elected operator.

use chrono::Duration;
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::params::StressLevel;
use engine_core::protocol::ProtocolVersion;
use engine_core::stress::STRESS_FLOOR_LAPSE_MS;
use engine_core::types::MarketId;

use crate::common::{BTC, ETH, Harness, f};
use crate::v37::ELECTED_SEED;

/// A benchmark no market lists, so its rates finance nothing the later parts hold.
const BENCHMARK: &str = "V46-RATE";

/// The operator's stress floor in `market`, which the automatic level may stand above.
fn floor_in(h: &Harness, market: MarketId) -> usize {
    h.engine.markets()[&market].stress.floor
}

/// A snapshot of `rates` for the benchmark, as `(days ago, annual rate)`, as the operator signs it.
fn snapshot(h: &mut Harness, rates: &[(i64, &str)]) -> Vec<Event> {
    let as_of = h.engine.benchmark_date(h.ts);
    let mut checked = h.engine.benchmark_state().required;
    checked.insert(BENCHMARK.into());
    let rates = rates
        .iter()
        .map(|(days, rate)| BenchmarkRate {
            benchmark: BENCHMARK.into(),
            effective_date: as_of - Duration::days(*days),
            annual_rate: f(rate),
        })
        .collect();
    h.operate_as(ELECTED_SEED, AdminCommand::SetBenchmarkRates(BenchmarkBatch { as_of, checked, rates }))
}

/// `V46` activates with a floor above level 1 in force, which drops to level 1 and starts its 72
/// hours. A floor above level 1 is refused, in one market and in all, and one at level 1 applies; a
/// floor set again restarts its time, and each lapses at its 72 hours. A benchmark's first rate is
/// bound by its range alone, a move within the limit applies, a move past it refuses the whole
/// snapshot, and a gap between dates scales the limit. A signed undated rate is held to the same cap,
/// and a market holding positions cannot be listed again on the new benchmark.
pub(crate) fn session(h: &mut Harness) {
    // Levels only an operator's floor reaches, so a floor above level 1 names one that exists.
    let mut parameters = h.engine.params().expect("the session sets parameters").clone();
    while parameters.stress_levels.len() < 3 {
        let last = parameters.stress_levels.last().expect("the normal level").clone();
        let raised = StressLevel { min_spread_bps: last.min_spread_bps + 5, trigger: None, ..last };
        parameters.stress_levels.push(raised);
    }
    assert_eq!(h.operate_as(ELECTED_SEED, AdminCommand::SetParams(parameters)).last(), Some(&Event::AdminApplied));

    let floor = |market, level| AdminCommand::SetStressFloor { market, level };
    assert_eq!(h.operate_as(ELECTED_SEED, floor(Some(BTC), 2)).last(), Some(&Event::AdminApplied));
    assert_eq!(floor_in(h, BTC), 2);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V46 });
    assert_eq!(floor_in(h, BTC), 1);
    let activated = h.ts;

    for market in [Some(ETH), None] {
        let refused = h.operate_as(ELECTED_SEED, floor(market, 2));
        assert!(matches!(refused.last(), Some(Event::AdminRejected { .. })), "{refused:?}");
    }
    assert_eq!(h.operate_as(ELECTED_SEED, floor(None, 1)).last(), Some(&Event::AdminApplied));
    h.advance(1_000);
    assert_eq!(h.operate_as(ELECTED_SEED, floor(Some(ETH), 1)).last(), Some(&Event::AdminApplied));
    let renewed = h.ts;

    // BTC's and the global floor's 72 hours run first, ETH's a second later.
    h.advance_to(activated + STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    assert_eq!((floor_in(h, BTC), floor_in(h, ETH)), (0, 1));
    h.advance_to(renewed + STRESS_FLOOR_LAPSE_MS);
    h.cmd(Command::Tick);
    assert_eq!(floor_in(h, ETH), 0);

    assert_eq!(snapshot(h, &[(3, "0.9")]).last(), Some(&Event::AdminApplied));
    assert_eq!(snapshot(h, &[(2, "0.87")]).last(), Some(&Event::AdminApplied));
    let refused = snapshot(h, &[(1, "0.8")]);
    assert!(matches!(refused.last(), Some(Event::AdminRejected { .. })), "{refused:?}");
    assert_eq!(snapshot(h, &[(0, "0.81")]).last(), Some(&Event::AdminApplied));

    let undated = |annual_bps| AdminCommand::SetBenchmarkRate { benchmark: BENCHMARK.into(), annual_bps };
    let refused = h.operate_as(ELECTED_SEED, undated(5_000));
    assert!(matches!(refused.last(), Some(Event::AdminRejected { .. })), "{refused:?}");

    let mut relisted = h.engine.markets()[&BTC].spec.clone();
    relisted.financing.benchmark = BENCHMARK.into();
    let refused = h.operate_as(ELECTED_SEED, AdminCommand::UpsertMarket(relisted));
    assert!(matches!(refused.last(), Some(Event::AdminRejected { .. })), "{refused:?}");
}
