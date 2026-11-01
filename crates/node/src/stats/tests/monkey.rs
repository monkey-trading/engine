use engine_core::emissions::TokenUnits;
use engine_core::types::Side;

use super::*;

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

/// A loss in the open period is allocated when it closes; the activity, the per-loss ratio and the
/// MONKEY view report what the events say.
#[test]
fn a_closed_period_shows_in_the_activity_the_ratio_and_the_totals() {
    let mut rig = Rig::new();
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 1);
    // The first 2M of notional trades at no spread, so the loss is the index falling under the long.
    rig.price(T0 + 1_500, 99_000);
    let sold = rig.trade(T0 + 2_000, TRADER, Side::Sell, 1);
    assert!(sold.iter().any(|e| matches!(e, Event::Fill(f) if f.realised_pnl.is_negative())), "{sold:?}");
    let closed = rig.apply(T0 + 600_000, Command::Tick);
    let allocated: Vec<_> = closed
        .iter()
        .filter_map(|event| match event {
            Event::EmissionAllocated { account, weight, amount, .. } => Some((*account, *weight, *amount)),
            _ => None,
        })
        .collect();
    assert_eq!(allocated.len(), 1, "{closed:?}");
    let (account, weight, amount) = allocated[0];
    assert_eq!(account, TRADER);
    assert!(weight.is_positive() && amount.0 > 0);

    let page = json(rig.stats.activity(None, 10).unwrap());
    let entry = &page["entries"][0];
    assert_eq!(entry["type"], "loss_close");
    assert_eq!(entry["account"], TRADER.to_string());
    assert_eq!(entry["amount"], amount.to_string());
    assert_eq!(entry["period"], 0);
    assert_eq!(page["from_genesis"], true);

    let query = crate::stats::SeriesQuery {
        metric: "monkey_per_loss".into(),
        market: None,
        resolution: "1h".into(),
        from_ms: None,
        to_ms: None,
        limit: None,
    };
    let series = json(rig.stats.series(&rig.engine, &query).unwrap());
    // One period, one allocation: the ratio is the amount over the weight, in base units per USD.
    let per: Fixed = series["entries"][0]["value"].as_str().unwrap().parse().unwrap();
    let expected = Fixed::from_int(i64::try_from(amount.0).unwrap()).div(weight);
    assert!((per - expected).abs() <= Fixed::from_raw(1_000), "{per} vs {expected}");
    assert_eq!(series["unit"], "monkey_base_units_per_usd");

    let monkey = json(rig.stats.monkey(&rig.engine));
    assert_eq!(monkey["emissions"]["allocated"], amount.to_string());
    assert_eq!(monkey["emissions"]["period_ms"], 600_000);
    assert_eq!(monkey["emissions"]["halving_periods"], 26_280);
    assert_eq!(monkey["emissions"]["period"], 1);
    assert_eq!(monkey["emissions"]["start_ms"], T0);
    assert!(monkey["staking"].is_null() && monkey["distribution"].is_null());
}

#[test]
fn activity_pages_newest_first_and_the_cursor_continues_where_a_page_stopped() {
    let mut rig = Rig::staking();
    for (n, who) in [TRADER, OTHER, TRADER].into_iter().enumerate() {
        let command = Command::Stake {
            account: who,
            amount: TokenUnits(100 * (n as u128 + 1)),
            block: 1,
            chain_ref: format!("0xs:{n}"),
        };
        rig.apply(T0 + 1_000 * (n as i64 + 1), command);
    }
    let first = json(rig.stats.activity(None, 2).unwrap());
    let amounts: Vec<_> = first["entries"].as_array().unwrap().iter().map(|e| e["amount"].clone()).collect();
    assert_eq!(amounts, ["300", "200"]);
    assert!(first["entries"].as_array().unwrap().iter().all(|e| e["type"] == "stake"));
    let second = json(rig.stats.activity(first["next"].as_u64(), 2).unwrap());
    assert_eq!(second["entries"][0]["amount"], "100");
    assert!(second["next"].is_null());
    assert!(rig.stats.activity(None, 0).is_err() && rig.stats.activity(None, 201).is_err());
}

#[test]
fn stakes_and_unstakes_move_the_totals_the_view_and_the_series_report() {
    let mut rig = Rig::staking();
    let stake = |n: u32, who, amount| Command::Stake {
        account: who,
        amount: TokenUnits(amount),
        block: 1,
        chain_ref: format!("0xs:{n}"),
    };
    rig.apply(T0 + 1_000, stake(0, TRADER, 1_500));
    rig.apply(T0 + 2_000, stake(1, OTHER, 500));
    let view = json(rig.stats.monkey(&rig.engine));
    assert_eq!(
        (view["staking"]["total_staked"].as_str(), view["staking"]["stakers"].as_u64()),
        (Some("2000"), Some(2))
    );
    rig.apply(
        T0 + 3_000,
        Command::Unstake { account: TRADER, amount: TokenUnits(1_500), block: 2, chain_ref: "0xu:0".into() },
    );
    let view = json(rig.stats.monkey(&rig.engine));
    assert_eq!((view["staking"]["total_staked"].as_str(), view["staking"]["stakers"].as_u64()), (Some("500"), Some(1)));
    assert_eq!(view["staking"]["cooldown_ms"], 3 * 86_400_000_i64);
    let query = crate::stats::SeriesQuery {
        metric: "total_staked".into(),
        market: None,
        resolution: "1m".into(),
        from_ms: None,
        to_ms: None,
        limit: None,
    };
    let series = json(rig.stats.series(&rig.engine, &query).unwrap());
    let bucket = &series["entries"][0];
    assert_eq!(
        (bucket["high"].as_str(), bucket["close"].as_str(), bucket["unit"].as_str()),
        (Some("2000"), Some("500"), None)
    );
    let page = json(rig.stats.activity(None, 10).unwrap());
    assert_eq!(page["entries"][0]["type"], "unstake");
}

/// An epoch that closes above the floor distributes; the view names it and the series count it.
#[test]
fn a_closed_epoch_is_reported_with_what_it_measured_and_distributed() {
    let mut rig = Rig::staking();
    rig.apply(
        T0 + 1_000,
        Command::Stake { account: TRADER, amount: TokenUnits(100), block: 1, chain_ref: "0xs:0".into() },
    );
    let end = rig.engine.staking().unwrap().config.epoch_end(0);
    let closed = rig.apply(end, Command::Tick);
    let Some(Event::DistributionEpochClosed { allocated, excess, .. }) =
        closed.iter().find(|event| matches!(event, Event::DistributionEpochClosed { .. }))
    else {
        panic!("the epoch did not close: {closed:?}");
    };
    assert!(allocated.0 > 0);
    let view = json(rig.stats.monkey(&rig.engine));
    let distribution = &view["distribution"];
    assert_eq!(distribution["last_distribution"]["allocated"], allocated.to_string());
    assert_eq!(distribution["last_distribution"]["excess"], excess.to_string());
    assert_eq!(distribution["epoch"], 1);
    assert_eq!(distribution["last_closed_epoch"], 0);
    assert_eq!(distribution["last_closed_ms"], end);
    assert_eq!(distribution["next_close_ms"], end + 86_400_000);
    assert_eq!(distribution["distributed"], allocated.to_string());
    assert_eq!(distribution["liquidity_floor"], "1000");
    let query = crate::stats::SeriesQuery {
        metric: "distributed_per_epoch".into(),
        market: None,
        resolution: "1d".into(),
        from_ms: None,
        to_ms: None,
        limit: None,
    };
    let series = json(rig.stats.series(&rig.engine, &query).unwrap());
    assert_eq!(series["entries"][0]["value"], allocated.to_string());
    assert_eq!(series["unit"], "usdc_base_units");
}
