use engine_core::emissions::TokenUnits;
use engine_core::types::Side;

use super::*;

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

/// A recovery that replays no command (a snapshot at the log head) still serves the stake the engine
/// holds and a full scan, which the first command after it would otherwise have supplied.
#[test]
fn a_recovery_with_no_commands_serves_the_totals_and_a_scan_of_the_recovered_state() {
    let mut rig = Rig::staking();
    for (n, who, amount) in [(0, TRADER, 1_500), (1, OTHER, 500)] {
        let command =
            Command::Stake { account: who, amount: TokenUnits(amount), block: 1, chain_ref: format!("0xs:{n}") };
        rig.apply(T0 + 1_000, command);
    }
    let recovered = stats();
    recovered.restart();
    recovered.finish_recovery(&rig.engine);
    let monkey = json(recovered.monkey(&rig.engine));
    assert_eq!(monkey["staking"]["total_staked"], "2000");
    assert_eq!(monkey["staking"]["stakers"], 2);
    assert_eq!(monkey["distribution"]["excess_seq"], rig.engine.last_seq().unwrap());
    let house = json(recovered.house(&rig.engine));
    assert_eq!(house["seq"], rig.engine.last_seq().unwrap(), "{house}");
    // Nothing was recorded, so the answer says it begins when the node's first command does.
    let series = json(recovered.series(&rig.engine, &query("nav")).unwrap().finish());
    assert!(series["begins_ms"].is_null() && series["entries"].as_array().unwrap().is_empty(), "{series}");
}

/// A replay takes the cheap scan for the intervals nothing will read, so a long log recovers faster,
/// yet leaves the series the full scans leave, and ends with the full scan the answers read.
#[test]
fn a_replay_scans_lightly_and_leaves_the_series_a_live_node_holds() {
    let script = |rig: &mut Rig| {
        rig.trade(T0 + 1_000, TRADER, Side::Buy, 2);
        rig.trade(T0 + 2_000, OTHER, Side::Sell, 1);
        for minute in 1..=4 {
            rig.price(T0 + minute * 60_000, 100_000 + minute);
        }
    };
    let mut live = Rig::new();
    script(&mut live);

    let replayed = stats();
    replayed.restart();
    let mut rig = Rig::recording_into(replayed.clone());
    script(&mut rig);
    assert_eq!(json(replayed.house(&rig.engine))["snapshot_ms"], T0 + 4 * 60_000, "a light scan is the latest");
    assert_eq!(json(replayed.house(&rig.engine))["open_margin"], "0", "and takes no margin");
    assert_eq!(replayed.dump(), live.stats.dump(), "the series are the live node's");
    replayed.finish_recovery(&rig.engine);
    let house = json(replayed.house(&rig.engine));
    assert!(house["open_margin"].as_str().unwrap().parse::<Fixed>().unwrap().is_positive(), "{house}");
    assert_eq!(json(live.stats.house(&live.engine))["open_margin"], house["open_margin"]);
}

/// A record that starts at a command after the log's first says so, as a recovery from a snapshot does.
#[test]
fn a_record_that_starts_after_the_first_command_does_not_claim_the_genesis() {
    let mut rig = Rig::new();
    rig.price(T0 + 1_000, 100_050);
    let late = stats();
    late.observe(&rig.engine, T0 + 1_000, &[]);
    let series = json(late.series(&rig.engine, &query("cash")).unwrap().finish());
    assert_eq!(series["from_genesis"], false);
    assert_eq!(series["begins_seq"], rig.engine.last_seq().unwrap());
    assert_eq!(json(late.activity(None, 5).unwrap())["from_genesis"], false);
}

fn query(metric: &str) -> crate::stats::SeriesQuery {
    crate::stats::SeriesQuery {
        metric: metric.into(),
        market: None,
        resolution: "1m".into(),
        from_ms: None,
        to_ms: None,
        limit: None,
    }
}
