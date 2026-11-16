//! The futures warnings: a market on its last listed contract, and one past a roll time with no gap
//! recorded.

use chrono::DateTime;
use engine_core::futures::FuturesContract;
use engine_core::market::MarketSpec;

use super::*;

#[test]
fn a_futures_market_on_its_last_listed_contract_warns_once_until_a_next_is_listed() {
    let mut last = LastContract::default();
    let on = |contract: Option<&str>| {
        let market = MarketSample { last_contract: contract.map(Into::into), ..market_sample(20, true, Ok(())) };
        sample(vec![market])
    };
    let mut check = |sample: Sample| capture(|| last.check(&sample));
    assert!(check(on(None)).is_empty());
    let [alert] = check(on(Some("ESH8"))).try_into().unwrap();
    assert_eq!((alert.key(), alert.severity), ("futures_last_contract{market=20}".to_owned(), Some(Severity::Warning)));
    assert!(check(on(Some("ESH8"))).is_empty());
    let [resolve] = check(on(None)).try_into().unwrap();
    assert_eq!((resolve.key(), resolve.severity), (alert.key(), None));
}

#[test]
fn a_sample_names_a_futures_markets_contract_only_while_it_is_the_last_listed() {
    let mut cfg = dev_config();
    let us500 = cfg.genesis.markets.iter().find(|m| m.id == 20).expect("US 500").clone();
    let es = |name: &str, from: Millis| FuturesContract {
        contract: name.into(),
        from: DateTime::from_timestamp_millis(from).expect("in range"),
    };
    let day = 86_400_000;
    let last = es("ESZ6", T0 - day);
    let with_next = vec![last.clone(), es("ESH7", T0 + day)];
    for (id, contracts) in [(70, vec![last]), (71, with_next)] {
        cfg.genesis.markets.push(MarketSpec { id, symbol: format!("ES{id}"), contracts, ..us500.clone() });
    }
    let engine = engine_of(&cfg);
    assert_eq!(market(&engine, 70).last_contract.as_deref(), Some("ESZ6"));
    assert_eq!(market(&engine, 71).last_contract, None);
    assert_eq!(market(&engine, 20).last_contract, None);
}

#[test]
fn a_futures_market_past_a_roll_time_with_no_gap_warns_once_until_it_rolls() {
    let mut cfg = dev_config();
    let us500 = cfg.genesis.markets.iter().find(|m| m.id == 20).expect("US 500").clone();
    let es = |name: &str, from: Millis| FuturesContract {
        contract: name.into(),
        from: DateTime::from_timestamp_millis(from).expect("in range"),
    };
    let contracts = vec![es("ESZ6", T0 - 86_400_000), es("ESH7", T0 + 3_600_000)];
    cfg.genesis.markets.push(MarketSpec { id: 70, symbol: "ES70".into(), contracts, ..us500 });
    let mut engine = engine_of(&cfg);
    let quote = |contract: &str, ts: Millis| {
        let quote = |source: &str| SourceQuote {
            source: source.to_owned(),
            mid: Fixed::from_int(5_000),
            ts,
            contract: Some(contract.into()),
        };
        Command::Prices { market: 70, quotes: vec![quote("venue-a"), quote("venue-b")] }
    };
    let mut overdue = RollOverdue::default();
    let mut check = |engine: &Engine| capture(|| overdue.check(&Sample::of(engine)));
    // Before the roll time nothing is overdue, with or without a gap.
    sequence(&mut engine, T0, quote("ESZ6", T0));
    assert!(check(&engine).is_empty());
    // Past it, with only the front quoting, no gap is recorded: one Warning, not one per sample.
    let due = T0 + 3_600_000;
    sequence(&mut engine, due, quote("ESZ6", due));
    assert_eq!(market(&engine, 70).roll_overdue.as_deref(), Some("ESH7"));
    let [alert] = check(&engine).try_into().unwrap();
    assert_eq!((alert.key(), alert.severity), ("futures_roll_overdue{market=70}".to_owned(), Some(Severity::Warning)));
    assert!(check(&engine).is_empty());
    // The next contract quotes, the market rolls, and the alert resolves.
    let events = sequence(&mut engine, due, quote("ESH7", due));
    assert!(events.iter().any(|e| matches!(e, Event::MarketRolled { market: 70, .. })), "{events:?}");
    let [resolve] = check(&engine).try_into().unwrap();
    assert_eq!((resolve.key(), resolve.severity), (alert.key(), None));
    assert!(check(&engine).is_empty());
}
