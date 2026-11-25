use std::time::Duration;

use engine_core::Engine;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::staking::UsdcUnits;
use engine_core::turns::TurnReason;
use engine_core::types::{AccountId, Millis, Side};

use super::*;
use crate::stats::tests::{BTC, OTHER, Rig, T0, TRADER};
use crate::stats::{SeriesQuery, Stats};
use crate::test_support::{LOADED_WAIT, stats};

/// A record taken partway through, encoded as a daily snapshot carries it and restored, then fed the rest, answers
/// as the record that saw every command: the same series, activity, distributions, turns and MONKEY totals, and
/// the same beginning.
#[test]
fn a_restored_record_answers_as_the_record_it_was_taken_from() {
    let mut whole = Rig::new();
    before(&mut whole);
    after(&mut whole);

    let mut taken = Rig::new();
    before(&mut taken);
    let record = taken.stats.capture().wait(LOADED_WAIT).expect("no scan is in flight");
    let encoded = serde_json::to_string(&record).unwrap();
    let restored = stats();
    restored.restore(serde_json::from_str(&encoded).unwrap());
    let mut resumed = Rig { engine: taken.engine, stats: restored };
    after(&mut resumed);
    // A start ends with a scan of the state it reached.
    resumed.stats.finish_recovery(&resumed.engine);

    assert_eq!(resumed.stats.dump(), whole.stats.dump());
    assert_eq!(answers(&resumed.stats, &resumed.engine), answers(&whole.stats, &whole.engine));
    assert!(whole.stats.dump().contains("MonkeyPerLoss"), "the record holds a ratio");
}

/// A capture waits for the scans due by then that the worker has not finished, and holds their books once they
/// are installed, as the record does.
#[test]
fn a_capture_holds_the_scans_in_flight_once_they_are_installed() {
    let mut rig = Rig::new();
    rig.stats.scan_in_background(&rig.engine);
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 2);
    // Due a light scan, which the worker takes only once the batch is delivered.
    rig.price(T0 + 6_000, 101_000);
    let early = rig.stats.capture();
    assert!(early.wait(Duration::ZERO).is_none(), "a capture is complete before the scan it waits for");
    let capture = rig.stats.capture();
    rig.stats.settle();
    let record = capture.wait(LOADED_WAIT).expect("the scan completes the capture");
    let restored = stats();
    restored.restore(record);
    assert!(rig.stats.dump().contains("Long"), "the scan sampled the books");
    assert_eq!(restored.dump(), rig.stats.dump());
}

/// A capture whose worker stops is handed over as it stands, as the record goes on without the scans it waited for.
#[test]
fn a_capture_is_handed_over_when_the_worker_stops() {
    let mut rig = Rig::new();
    rig.stats.scan_in_background(&rig.engine);
    rig.price(T0 + 6_000, 101_000);
    let capture = rig.stats.capture();
    rig.stats.lock().worker_stopped("stopped by the test");
    let restored = stats();
    restored.restore(capture.wait(Duration::ZERO).expect("handed over as it stands"));
    assert_eq!(restored.dump(), rig.stats.dump());
}

/// A record restored under smaller rings keeps their newest entries, and says older ones were dropped.
#[test]
fn a_restored_record_keeps_this_nodes_ring_sizes() {
    let rig = Rig::new();
    for epoch in 0..3 {
        rig.stats.observe(&rig.engine, T0 + 1 + epoch as Millis, &[closed(epoch)]);
    }
    let record = rig.stats.capture().wait(LOADED_WAIT).unwrap();
    let mut cfg = crate::test_support::dev_config();
    cfg.stats.distribution_limit = 2;
    let restored = Stats::new(&cfg).unwrap();
    restored.restore(record);
    let page = serde_json::to_value(restored.distributions(&rig.engine, None, 10).unwrap()).unwrap();
    let epochs: Vec<u64> = page["epochs"].as_array().unwrap().iter().map(|e| e["epoch"].as_u64().unwrap()).collect();
    assert_eq!((epochs, &page["truncated"]), (vec![2, 1], &serde_json::Value::Bool(true)));
}

/// A record whose series name a bucket of another shape than their metric's does not decode.
#[test]
fn a_bucket_of_another_shape_than_its_metrics_is_refused() {
    let mut rig = Rig::new();
    rig.price(T0 + 1_000, 101_000);
    let record = rig.stats.capture().wait(LOADED_WAIT).unwrap();
    let mut encoded = serde_json::to_value(&record).unwrap();
    let nav = encoded["series"]["series"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|series| series["metric"] == "nav")
        .expect("the record holds the NAV");
    nav["buckets"][0][0][1] = serde_json::json!(["1"]);
    let error = serde_json::from_value::<Record>(encoded).err().expect("refused").to_string();
    assert!(error.contains("another shape"), "{error}");
}

// ── Helpers ──

/// The commands before the record is taken: a round trip, an emission period's close, an epoch's close and a turn.
fn before(rig: &mut Rig) {
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 2);
    rig.price(T0 + 1_500, 99_000);
    rig.trade(T0 + 2_000, TRADER, Side::Sell, 2);
    rig.apply(T0 + 600_000, Command::Tick);
    rig.stats.observe(&rig.engine, T0 + 600_001, &[closed(0), assigned(5)]);
}

/// The commands after it: another round trip, a later period's close, an epoch's close, and the turn taken.
fn after(rig: &mut Rig) {
    rig.price(T0 + 700_000, 101_000);
    rig.trade(T0 + 701_000, OTHER, Side::Sell, 1);
    rig.price(T0 + 702_000, 98_000);
    rig.trade(T0 + 703_000, OTHER, Side::Buy, 1);
    rig.apply(T0 + 1_200_000, Command::Tick);
    let taken = Event::TurnTaken { turn: 5, candidate: MEMBER, release: engine_core::eip712::B256::repeat_byte(3) };
    rig.stats.observe(&rig.engine, T0 + 1_200_001, &[closed(1), taken]);
}

const MEMBER: AccountId = AccountId([0xc1; 20]);

/// An epoch's close, which allocated 600 USDC base units.
fn closed(epoch: u64) -> Event {
    Event::DistributionEpochClosed {
        epoch,
        nav: engine_core::Fixed::from_int(1_500),
        floor: engine_core::Fixed::from_int(1_000),
        excess: UsdcUnits(u128::from(u64::MAX) + 7),
        allocated: UsdcUnits(600),
        awarded: UsdcUnits(0),
    }
}

/// A turn's draw.
fn assigned(turn: u64) -> Event {
    Event::TurnAssigned {
        turn,
        position: 0,
        reason: TurnReason::Draw,
        candidate: MEMBER,
        admin_key: AccountId([0xa1; 20]),
        applies_at: T0 + 86_400_000,
    }
}

/// What the views answer from `stats` over `engine`: the series of each kind, the activity, the turns, the
/// distributions and the MONKEY view, but for the excess a scan measures, which a start takes afresh.
fn answers(stats: &Stats, engine: &Engine) -> Vec<serde_json::Value> {
    let series = |metric: &str, market, resolution: &str| {
        let query = SeriesQuery {
            metric: metric.into(),
            market,
            resolution: resolution.into(),
            from_ms: None,
            to_ms: None,
            limit: None,
        };
        serde_json::to_value(stats.series(engine, &query).unwrap().finish()).unwrap()
    };
    let mut monkey = serde_json::to_value(stats.monkey(engine)).unwrap();
    if let Some(distribution) = monkey["distribution"].as_object_mut() {
        distribution.remove("excess_usd");
        distribution.remove("excess_seq");
    }
    vec![
        series("nav", None, "1m"),
        series("volume", Some(BTC), "1h"),
        series("long", Some(BTC), "1m"),
        series("monkey_per_loss", None, "1d"),
        serde_json::to_value(stats.activity(None, 50).unwrap()).unwrap(),
        serde_json::to_value(stats.turns(None, 50).unwrap()).unwrap(),
        serde_json::to_value(stats.distributions(engine, None, 50).unwrap()).unwrap(),
        monkey,
    ]
}
