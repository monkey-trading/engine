use engine_core::types::Side;

use super::*;
use crate::stats::background::Background;

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

/// The commands both records see: trades on both sides of a scan falling due, then more after it, so the
/// state has moved on by the time the background scan is installed.
fn script(rig: &mut Rig) {
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 2);
    rig.trade(T0 + 2_000, OTHER, Side::Sell, 1);
    rig.price(T0 + 30_000, 101_000);
    rig.price(T0 + 60_000, 102_000);
    rig.trade(T0 + 61_000, TRADER, Side::Sell, 2);
    rig.price(T0 + 62_000, 90_000);
}

/// The scan a worker takes is the one an inline record takes at the command it fell due at: the same
/// figures at the same seq and time, the same series, though the engine moved on before it landed.
#[test]
fn a_background_scan_gives_what_an_inline_scan_gives_at_the_command_it_fell_due_at() {
    let mut inline = Rig::new();
    script(&mut inline);

    let mut background = Rig::new();
    background.stats.scan_in_background(&background.engine);
    script(&mut background);
    background.stats.settle();

    let (served_inline, served) =
        (json(inline.stats.house(&inline.engine)), json(background.stats.house(&background.engine)));
    assert_eq!(served["seq"], served_inline["seq"]);
    assert_eq!(served["snapshot_ms"], T0 + 60_000);
    assert!(served["open_interest"].as_str().unwrap().parse::<Fixed>().unwrap().is_positive(), "{served}");
    assert_eq!(served["open_interest"], served_inline["open_interest"]);
    assert_eq!(background.stats.dump(), inline.stats.dump());
    let positions = |rig: &Rig| json(rig.stats.positions(&rig.engine, BTC).unwrap());
    assert_eq!(positions(&background), positions(&inline));
}

/// Every due scan is taken, whatever the worker's speed, so the series are the ones replay gives; the last
/// one is served.
#[test]
fn every_due_scan_is_taken_and_the_last_is_served() {
    let mut rig = Rig::new();
    rig.stats.scan_in_background(&rig.engine);
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 1);
    rig.price(T0 + 60_000, 101_000);
    rig.price(T0 + 120_000, 102_000);
    rig.stats.settle();
    assert_eq!(json(rig.stats.house(&rig.engine))["snapshot_ms"], T0 + 120_000);
    let query = SeriesQuery {
        metric: "long".into(),
        market: Some(BTC),
        resolution: "1m".into(),
        from_ms: None,
        to_ms: None,
        limit: None,
    };
    let series = json(rig.stats.series(&rig.engine, &query).unwrap().finish());
    assert_eq!(series["entries"].as_array().unwrap().len(), 2, "{series}");
}

/// A worker whose copy cannot follow the sequencer says so, and never serves a scan of a state the
/// sequencer was not in.
#[test]
fn a_worker_given_a_command_out_of_sequence_stops_without_scanning() {
    let rig = Rig::new();
    let mut worker = Background::start(&rig.engine).unwrap();
    let skipped = Sequenced { seq: rig.engine.last_seq().unwrap() + 5, ts: T0 + 1_000, cmd: Command::Tick };
    assert!(worker.apply(&skipped));
    let stopped = worker.wait().expect_err("the worker refuses the gap");
    assert!(stopped.contains("applying seq"), "{stopped}");
}
