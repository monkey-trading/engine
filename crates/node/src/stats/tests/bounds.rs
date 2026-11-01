use engine_core::event::Event;
use engine_core::types::Side;

use super::*;
use crate::stats::SeriesQuery;

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

fn query(metric: &str, market: Option<MarketId>, resolution: &str) -> SeriesQuery {
    SeriesQuery {
        metric: metric.into(),
        market,
        resolution: resolution.into(),
        from_ms: None,
        to_ms: None,
        limit: None,
    }
}

/// The scan of every account runs once an `api.stats_interval` (60 s in the dev config) after the last
/// one, wherever the commands fall: a scan late in its window does not bring the next forward.
#[test]
fn scans_are_a_whole_interval_apart_whenever_the_commands_arrive() {
    let mut rig = Rig::new();
    let scanned = |rig: &Rig| json(rig.stats.house(&rig.engine))["snapshot_ms"].as_i64();
    assert_eq!(scanned(&rig), None, "no scan before the first interval has passed");
    rig.price(T0 + 59_999, 100_001);
    assert_eq!(scanned(&rig), None);
    rig.price(T0 + 100_000, 100_002);
    assert_eq!(scanned(&rig), Some(T0 + 100_000), "the first command past the interval scans");
    rig.price(T0 + 159_999, 100_003);
    assert_eq!(scanned(&rig), Some(T0 + 100_000), "not a boundary, an interval after the last scan");
    rig.price(T0 + 160_000, 100_004);
    assert_eq!(scanned(&rig), Some(T0 + 160_000));
}

/// A series nothing is recorded to still ages out with the sequenced time, as a flow does after its
/// last fill.
#[test]
fn a_series_with_no_new_data_loses_buckets_older_than_its_retention() {
    let mut rig = Rig::new();
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 1);
    let entries = |rig: &Rig, resolution: &str| {
        json(rig.stats.series(&rig.engine, &query("volume", Some(BTC), resolution)).unwrap().finish())["entries"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!((entries(&rig, "1m"), entries(&rig, "1h")), (1, 1));
    // 49 hours on, with no fill since: the minute bucket is past its 48 hours, the hourly one is not.
    rig.apply(T0 + 49 * 3_600_000, Command::Tick);
    assert_eq!((entries(&rig, "1m"), entries(&rig, "1h")), (0, 1));
    rig.apply(T0 + 91 * 24 * 3_600_000, Command::Tick);
    assert_eq!(entries(&rig, "1h"), 0, "and the hourly one past its 90 days");
    assert_eq!(entries(&rig, "1d"), 1, "while daily buckets are kept without limit");
}

/// The withdrawal queue's total follows the queued and approved amounts the events carry.
#[test]
fn the_queue_total_follows_the_queued_and_approved_amounts() {
    let rig = Rig::new();
    let (a, b) = (TRADER, OTHER);
    let ts = T0 + 1_000;
    let amounts = |queued: &[i64], approved: &[i64]| -> Vec<Event> {
        let queued = queued.iter().enumerate().map(|(n, amount)| Event::WithdrawalQueued {
            account: a,
            amount: Fixed::from_int(*amount),
            requested_seq: n as u64,
        });
        let approved = approved.iter().map(|amount| Event::WithdrawalApproved {
            account: b,
            amount: Fixed::from_int(*amount),
            nonce: 1,
        });
        queued.chain(approved).collect()
    };
    let close = |rig: &Rig| -> Fixed {
        let page = json(rig.stats.series(&rig.engine, &query("withdrawal_queue", None, "1m")).unwrap().finish());
        page["entries"].as_array().unwrap().last().unwrap()["close"].as_str().unwrap().parse().unwrap()
    };
    rig.stats.observe(&rig.engine, ts, &amounts(&[60, 40], &[]));
    assert_eq!(close(&rig), Fixed::from_int(100));
    rig.stats.observe(&rig.engine, ts + 1, &amounts(&[], &[30]));
    assert_eq!(close(&rig), Fixed::from_int(70));
    // A request approved in the command that queues it leaves nothing.
    rig.stats.observe(&rig.engine, ts + 2, &amounts(&[25], &[25]));
    assert_eq!(close(&rig), Fixed::from_int(70));
}

/// The excess a close distributes is the engine's, not NAV over the floor: house cash that clients'
/// open losses have not yet paid in is not distributable.
#[test]
fn the_distribution_excess_is_the_engines_and_comes_with_the_scan_it_was_read_at() {
    let mut rig = Rig::staking();
    let excess = |rig: &Rig| json(rig.stats.monkey(&rig.engine))["distribution"].clone();
    assert!(excess(&rig)["excess"].is_null(), "no scan yet: {}", excess(&rig));
    rig.apply(T0 + 60_000, Command::Tick);
    let view = excess(&rig);
    let expected = rig.engine.distributable_excess().unwrap();
    assert_eq!(view["excess"].as_str().unwrap().parse::<Fixed>().unwrap(), expected);
    assert!(expected.is_positive());
    assert_eq!(view["excess_seq"], rig.engine.last_seq().unwrap());
}
