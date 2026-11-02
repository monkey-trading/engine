use engine_core::types::Side;

use super::*;
use crate::stats::SeriesQuery;

/// The last withdrawal queue total `stats` holds.
fn recorded(stats: &Stats, engine: &Engine) -> Fixed {
    let query = SeriesQuery {
        metric: "withdrawal_queue".into(),
        market: None,
        resolution: "1m".into(),
        from_ms: None,
        to_ms: None,
        limit: None,
    };
    let page = serde_json::to_value(stats.series(engine, &query).unwrap().finish()).unwrap();
    page["entries"].as_array().unwrap().last().unwrap()["close"].as_str().unwrap().parse().unwrap()
}

fn queued(engine: &Engine) -> Fixed {
    engine.withdrawal_queue().map(|request| request.remaining).sum()
}

/// A winner withdraws more than the house holds in cash, so part of the request waits; later cash
/// pays it down. The record's total is the engine's at every step, including in a record that only
/// begins once the queue is not empty.
#[test]
fn the_queue_total_is_the_engines_through_a_partial_approval_and_a_late_record() {
    let mut rig = Rig::new();
    // Take the house's physical cash down to the deposits, so the winner's profit cannot all be paid.
    let drained =
        rig.apply(T0, Command::HouseCapital { amount: -Fixed::from_int(9_999_990), chain_ref: "drain".into() });
    assert!(drained.iter().any(|event| matches!(event, Event::HouseCapitalMoved { .. })), "{drained:?}");
    let bought = rig.trade(T0 + 1_000, TRADER, Side::Buy, 2);
    assert!(bought.iter().any(|event| matches!(event, Event::Fill(_))), "{bought:?}");
    let (mut price, mut at) = (100_000, T0 + 2_000);
    while price < 170_000 {
        price = (price + 200).min(170_000);
        rig.price(at, price);
        at += 2_000;
    }
    // The last quote is two seconds old by now: quote again, then sell.
    rig.price(at, 170_000);
    let sold = rig.trade(at + 100, TRADER, Side::Sell, 2);
    assert!(sold.iter().any(|event| matches!(event, Event::Fill(_))), "{sold:?}");

    let withdrawal = Command::Withdraw { account: TRADER, amount: Fixed::from_int(225_000), auth: None };
    let events = rig.apply(at + 300, withdrawal);
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. })), "{events:?}");
    assert!(queued(&rig.engine).is_positive(), "part of the request waits: {events:?}");
    assert_eq!(recorded(&rig.stats, &rig.engine), queued(&rig.engine));

    // A record that begins now takes the queue as the engine holds it, and follows it from there.
    let late = stats();
    late.observe(&rig.engine, at + 400, &[]);
    assert_eq!(recorded(&late, &rig.engine), queued(&rig.engine));
    let deposit = Command::Deposit { account: OTHER, amount: Fixed::from_int(20_000), chain_ref: "pays-down".into() };
    let events = rig.apply(at + 500, deposit);
    late.observe(&rig.engine, at + 500, &events);
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. })), "{events:?}");
    assert_eq!(recorded(&rig.stats, &rig.engine), queued(&rig.engine));
    assert_eq!(recorded(&late, &rig.engine), queued(&rig.engine));
}
