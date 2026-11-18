//! Protocol plumbing: replay, sequencing, custody references, genesis, and admin validation, end
//! to end through `Engine::apply`.

mod common;

use chrono::{TimeZone, Utc};

use common::events::rejection;
use common::{BTC, Harness, acct, f, market, params};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::event::{Event, Reject};
use engine_core::market::MarketSpec;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, Side};
use engine_core::{Engine, Fixed, SequenceError};

#[test]
fn replaying_the_log_reproduces_the_state_root() {
    let mut h = Harness::new();
    let a = acct(1);
    h.deposit(a, "10000");
    h.price(BTC, "100000");
    h.market_order(a, BTC, Side::Buy, "0.05", "100000");
    h.order(a, BTC, Side::Sell, "0.05", OrderKind::Limit { price: f("105000") });
    h.price(BTC, "106000");

    let mut replica = Engine::new();
    for s in &h.log {
        replica.apply(s).unwrap();
    }
    assert_eq!(replica.state_root(), h.engine.state_root());

    // The log must be applied in order.
    let mut out_of_order = Engine::new();
    assert!(out_of_order.apply(&h.log[1]).is_err());
}

#[test]
fn the_engine_refuses_gaps_and_time_running_backwards() {
    let mut engine = Engine::new();
    let at = |seq, ts| Sequenced { seq, ts, cmd: Command::Tick };
    assert_eq!(engine.apply(&at(1, 0)), Err(SequenceError::Gap { expected: 0, got: 1 }));
    engine.apply(&at(0, 1_000)).unwrap();
    assert_eq!(engine.apply(&at(1, 999)), Err(SequenceError::TimeReversed { last: 1_000, got: 999 }));
    // A refused command changes nothing: the next valid one still lands at seq 1.
    assert!(engine.apply(&at(1, 1_000)).is_ok());
}

#[test]
fn the_engine_refuses_a_stamp_outside_the_range_it_accepts() {
    // SetParams schedules the first rollover from the stamp, which panics on one chrono cannot
    // represent; the command is refused instead, and the refusal names its seq.
    let mut engine = Engine::new();
    let bad = Sequenced { seq: 0, ts: i64::MAX, cmd: Command::Admin(AdminCommand::SetParams(params())) };
    assert_eq!(engine.apply(&bad), Err(SequenceError::StampOutOfRange { seq: 0, got: i64::MAX }));
    // The refusal changed nothing: valid commands still land, and a refusal past seq 0 names
    // the seq it refused.
    assert!(engine.apply(&Sequenced { seq: 0, ts: 1_000, cmd: Command::Tick }).is_ok());
    let bad = Sequenced { seq: 1, ts: i64::MAX, cmd: Command::Tick };
    assert_eq!(engine.apply(&bad), Err(SequenceError::StampOutOfRange { seq: 1, got: i64::MAX }));
    assert!(engine.apply(&Sequenced { seq: 1, ts: 1_000, cmd: Command::Tick }).is_ok());
}

#[test]
fn the_latest_accepted_stamp_is_the_end_of_9999() {
    // `MAX_STAMP_MS` is 9999-12-31T23:59:59.999Z, a conservative bound well inside chrono's
    // range: the rollover walk always has a next day to step to, whatever a market's time zone
    // does to the wall clock. The latest accepted stamp applies — `SetParams` schedules its
    // rollover — and one millisecond later is refused.
    let max = Utc.with_ymd_and_hms(9999, 12, 31, 23, 59, 59).unwrap().timestamp_millis() + 999;
    let set = |ts| Sequenced { seq: 0, ts, cmd: Command::Admin(AdminCommand::SetParams(params())) };
    let mut engine = Engine::new();
    let events = engine.apply(&set(max)).expect("the latest accepted stamp applies");
    assert!(matches!(events.last(), Some(Event::AdminApplied)), "{events:?}");
    let mut engine = Engine::new();
    assert_eq!(engine.apply(&set(max + 1)), Err(SequenceError::StampOutOfRange { seq: 0, got: max + 1 }));
}

#[test]
fn a_deposit_reference_is_credited_once() {
    let mut h = Harness::new();
    let a = acct(1);
    let deposit = |amount: &str| Command::Deposit { account: a, amount: f(amount), chain_ref: "0xtx:7".into() };
    assert!(matches!(h.cmd(deposit("500"))[..], [Event::Deposited { .. }]));
    // The custody adapter delivering the same deposit twice must not double-credit.
    let ev = h.cmd(deposit("500"));
    assert!(matches!(&ev[..], [Event::DepositRejected { reason: Reject::DuplicateReference, .. }]), "{ev:?}");
    assert_eq!(h.balance(a), f("500"));
}

#[test]
fn a_reference_counts_as_credited_only_once_a_credit_succeeds() {
    let mut h = Harness::new();
    assert!(h.engine.is_credited("genesis"));
    assert!(!h.engine.is_credited("0xtx:1"));
    // A refused deposit leaves its reference free, so a valid redelivery can still credit it.
    h.cmd(Command::Deposit { account: acct(1), amount: f("0"), chain_ref: "0xtx:1".into() });
    assert!(!h.engine.is_credited("0xtx:1"));
    h.cmd(Command::Deposit { account: acct(1), amount: f("5"), chain_ref: "0xtx:1".into() });
    assert!(h.engine.is_credited("0xtx:1"));
}

#[test]
fn credited_references_are_listed_by_prefix() {
    let mut h = Harness::new();
    for chain_ref in ["0xaa:1", "0xab:1", "0xaa:3", "0xa:1"] {
        h.cmd(Command::Deposit { account: acct(1), amount: f("1"), chain_ref: chain_ref.into() });
    }
    let refs: Vec<&str> = h.engine.credited_refs_with_prefix("0xaa:").collect();
    assert_eq!(refs, ["0xaa:1", "0xaa:3"]);
}

#[test]
fn deposits_out_of_range_are_rejected() {
    let mut h = Harness::new();
    for (i, amount) in ["0", "-1", "1000000000.000000001"].iter().enumerate() {
        let cmd = Command::Deposit { account: acct(1), amount: f(amount), chain_ref: format!("0xtx:{i}") };
        let ev = h.cmd(cmd);
        assert!(matches!(&ev[..], [Event::DepositRejected { reason: Reject::InvalidAmount, .. }]), "{amount}: {ev:?}");
    }
    assert!(h.engine.accounts().is_empty());
}

#[test]
fn house_capital_is_applied_once_and_never_drains_nav() {
    let mut h = Harness::new();
    let cash = h.engine.house_cash();
    // "genesis" was already applied by the harness.
    let ev = h.cmd(Command::HouseCapital { amount: f("1"), chain_ref: "genesis".into() });
    assert!(matches!(&ev[..], [Event::AdminRejected { .. }]));
    let ev = h.cmd(Command::HouseCapital { amount: f("-100000000"), chain_ref: "0xout".into() });
    assert!(matches!(&ev[..], [Event::AdminRejected { .. }]));
    assert_eq!(h.engine.house_cash(), cash);
}

#[test]
fn a_zero_house_capital_move_records_its_reference_only_from_v4() {
    let cases = [
        (ProtocolVersion::V2, Harness::new()),
        (ProtocolVersion::V3, Harness::v3(ClientAuth::Unsigned)),
        (ProtocolVersion::V4, Harness::v4(ClientAuth::Unsigned)),
    ];
    for (version, mut h) in cases {
        let cash = h.engine.house_cash();
        let zero = || Command::HouseCapital { amount: Fixed::ZERO, chain_ref: "zero".into() };
        let events = h.cmd(zero());
        if version >= ProtocolVersion::V4 {
            assert_eq!(events, [Event::HouseCapitalMoved { amount: Fixed::ZERO, nav: h.engine.house_nav() }]);
            assert!(h.engine.is_credited("zero"));
            let again = h.cmd(zero());
            assert!(matches!(&again[..], [Event::AdminRejected { reason }] if reason.contains("already applied")));
        } else {
            let refused = matches!(&events[..], [Event::AdminRejected { reason }] if reason.contains("out of range"));
            assert!(refused, "{version:?}: {events:?}");
            assert!(!h.engine.is_credited("zero"), "{version:?}");
        }
        assert_eq!(h.engine.house_cash(), cash, "{version:?}");
        // Moves of any other amount behave the same under every version.
        let one = h.cmd(Command::HouseCapital { amount: f("1"), chain_ref: "one".into() });
        assert!(matches!(&one[..], [Event::HouseCapitalMoved { .. }]), "{version:?}: {one:?}");
    }
}

#[test]
fn orders_before_genesis_or_for_unknown_markets_are_rejected() {
    let mut engine = Engine::new();
    let order = |market| {
        Command::PlaceOrder(OrderRequest {
            account: acct(1),
            client_id: 1,
            market,
            side: Side::Buy,
            qty: f("1"),
            kind: OrderKind::Market { quoted: f("1"), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
            take_profit: None,
            stop_loss: None,
        })
    };
    let ev = engine.apply(&Sequenced { seq: 0, ts: 0, cmd: order(BTC) }).unwrap();
    assert!(matches!(rejection(&ev), Reject::InvalidOrder { .. }));

    let mut h = Harness::new();
    h.deposit(acct(1), "100");
    assert_eq!(rejection(&h.cmd(order(99))), &Reject::UnknownMarket);
    let ev = h.price(99, "1");
    assert!(matches!(&ev[..], [Event::PricesRejected { market: 99, .. }]), "{ev:?}");
}

#[test]
fn invalid_admin_commands_change_nothing() {
    let mut h = Harness::new();
    let bad_market = MarketSpec { house_cap_bps: 0, ..market(9, "BAD", AssetClass::Index, Schedule::AlwaysOpen, 500) };
    let rejected = [
        AdminCommand::UpsertMarket(bad_market),
        AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 20_000 },
        AdminCommand::SetStressFloor { market: None, level: 5 },
        AdminCommand::SetStressFloor { market: Some(99), level: 1 },
        AdminCommand::SetReduceOnly { market: Some(99), reduce_only: true },
    ];
    let root = h.engine.state_root();
    for cmd in rejected {
        let ev = h.cmd(Command::Admin(cmd.clone()));
        assert!(matches!(&ev[..], [Event::AdminRejected { .. }]), "{cmd:?}: {ev:?}");
    }
    // Only the sequence position and time moved; no protocol state did.
    let mut replica = Engine::new();
    h.log.iter().take(h.log.len() - 5).for_each(|s| {
        replica.apply(s).unwrap();
    });
    assert_eq!(replica.state_root(), root);
    assert!(!h.engine.markets().contains_key(&9));
}

#[test]
fn listings_with_unknown_fields_fail_to_parse() {
    let mut spec = serde_json::to_value(market(9, "X", AssetClass::Index, Schedule::AlwaysOpen, 500)).unwrap();
    spec["house_capp_bps"] = serde_json::json!(5_000);
    assert!(serde_json::from_value::<MarketSpec>(spec).is_err());
    // The value itself still round-trips.
    let spec = market(9, "X", AssetClass::Index, Schedule::AlwaysOpen, 500);
    let back: MarketSpec = serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
    assert_eq!(back, spec);
    assert_eq!(back.min_order_notional, Fixed::ZERO);
}
