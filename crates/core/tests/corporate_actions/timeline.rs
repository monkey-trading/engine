use engine_core::Engine;
use engine_core::Fixed;
use engine_core::benchmark::BenchmarkBatch;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::params::Rollover;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::*;
use crate::support::*;

#[test]
fn quotes_held_across_a_closure_cannot_back_an_intermediate_opening_liquidation() {
    let mut h = held("0", "10");
    accepted(&schedule(&mut h, dividend("jump", AAPL, "1000", "0")));
    // One command jumps across the closure and the ex-open boundary. The debit applies at the
    // intermediate opening, but the pre-close quotes dropped at this command, so no close-out can
    // fill against a quote the hours rules already excluded.
    let events = tick(&mut h, open() + 1);
    assert!(events.iter().any(|e| matches!(e, Event::CorporateDividend { .. })), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    assert!(h.engine.markets()[&AAPL].quotes.is_empty(), "the held quotes drop at the jumping command");
    let events = h.price(AAPL, "80");
    assert!(
        events.iter().any(|e| matches!(e, Event::CloseOut { account, .. } if *account == acct(2))),
        "the first post-open quote closes the under-margined short: {events:?}"
    );
}

#[test]
fn a_rejected_benchmark_batch_crossing_a_closure_still_drops_held_quotes() {
    let mut h = held("2", "0");
    accepted(&schedule(&mut h, split("pending", 4, 1)));
    tick(&mut h, close() - 1);
    h.advance_to(open() + 1);
    let events = h.operate(AdminCommand::SetBenchmarkRates(BenchmarkBatch {
        as_of: "1900-01-01".parse().unwrap(),
        checked: ["SOFR".into()].into_iter().collect(),
        rates: vec![],
    }));
    assert!(events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::Fill(_) | Event::CloseOut { .. })), "{events:?}");
    assert!(
        h.engine.markets()[&AAPL].quotes.is_empty(),
        "the held quotes drop at the rejected batch's command, not at a later one"
    );
    assert!(h.engine.corporate_actions().contains_key("pending"));
}

#[test]
fn missed_rollovers_and_actions_run_in_chronological_economic_order() {
    let mut h = held("2", "0");
    let mut spec = h.engine.markets()[&AAPL].spec.clone();
    spec.financing.markup_bps = 3650;
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    accepted(&schedule(&mut h, dividend("first-cash", AAPL, "1", "0")));
    let mut second = split("later-split", 4, 1);
    second.previous_close = ny(2026, 10, 21, 16, 0);
    second.ex_open = ny(2026, 10, 22, 9, 30);
    accepted(&schedule(&mut h, second));
    let before = h.balance(acct(1));
    let events = tick(&mut h, ny(2026, 10, 23, 10, 0));
    let economic: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Financing { account, amount, market } if *account == acct(1) && *market == AAPL => {
                assert_eq!(*amount, f("0.16"));
                Some("rollover")
            }
            Event::CorporateDividend { .. } => Some("dividend"),
            Event::CorporateSplit { .. } => Some("split"),
            _ => None,
        })
        .collect();
    assert_eq!(economic, ["rollover", "dividend", "rollover", "split", "rollover"]);
    assert_eq!(h.balance(acct(1)), before + f("2") - f("0.48"));
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("8"));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn cash_closeouts_precede_later_missed_rollovers_in_a_single_timestamp_jump() {
    let mut h = held("0", "10");
    let mut spec = h.engine.markets()[&AAPL].spec.clone();
    spec.financing.markup_bps = 3650;
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    accepted(&schedule(&mut h, dividend("debit", AAPL, "1000", "0")));
    let events = tick(&mut h, ny(2026, 10, 23, 10, 0));
    let economic: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Financing { account, amount, .. } if *account == acct(2) => {
                assert_eq!(*amount, f("0.8"));
                Some("financing")
            }
            Event::CorporateDividend { .. } => Some("dividend"),
            Event::CloseOut { .. } => Some("closeout"),
            _ => None,
        })
        .collect();
    assert_eq!(economic, ["financing", "dividend", "closeout"]);
    assert!(h.engine.accounts()[&acct(2)].positions.is_empty());
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn captured_previous_close_quantity_survives_an_opening_fill_and_serialized_restart() {
    for restart_at in [close() - 1, close(), open() - 1] {
        let mut h = held("2", "3");
        accepted(&schedule(&mut h, dividend("restart", AAPL, "4", "0.25")));
        tick(&mut h, restart_at);
        let before = h.engine.state_root();
        h.engine = restored(&h.engine);
        assert_eq!(h.engine.state_root(), before);
        // A resting order placed while the venue is closed meets the opening quote in one command:
        // the captured pre-close quantities pay out before any opening fill.
        if h.ts < close() {
            h.advance_to(close());
        }
        let events = h.order(acct(1), AAPL, Side::Buy, "1", engine_core::command::OrderKind::Limit { price: f("80") });
        assert!(events.iter().any(|e| matches!(e, Event::OrderAccepted { .. })), "{events:?}");
        h.advance_to(open());
        let events = h.price(AAPL, "80");
        assert_eq!(payment(&events, 1), f("6"));
        assert_eq!(payment(&events, 2), f("-12"));
        assert_fill(&events);
        assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("3"));
        let adjustment = events.iter().position(|e| matches!(e, Event::CorporateDividend { .. })).unwrap();
        let fill = events.iter().position(|e| matches!(e, Event::Fill(_))).unwrap();
        assert!(adjustment < fill);
    }
}

#[test]
fn deterministic_replay_matches_application_events_and_incremental_state_leaves() {
    let mut h = held("2", "3");
    h.engine.state_root();
    let mut replay = restored(&h.engine);
    let first = h.log.len();
    accepted(&schedule(&mut h, dividend("cash", AAPL, "4", "0.25")));
    accepted(&schedule(&mut h, split("stock", 4, 1)));
    for at in [close(), open(), open() + 1] {
        tick(&mut h, at);
        let mut rebuilt = restored(&h.engine);
        assert_eq!(h.engine.state_root(), rebuilt.state_root());
    }
    let mut direct = restored(&replay);
    for command in &h.log[first..] {
        let a = replay.apply(command).unwrap();
        let b = direct.apply(command).unwrap();
        assert_eq!(a, b);
        assert_eq!(replay.state_root(), direct.state_root());
        direct = restored(&direct);
    }
    assert_eq!(replay.state_root(), h.engine.state_root());
    assert_eq!(serde_json::to_value(&replay).unwrap(), serde_json::to_value(&h.engine).unwrap());
}

#[test]
fn simultaneous_rollover_occurs_after_the_ex_date_adjustment() {
    let mut h = held("2", "0");
    let mut params = h.engine.params().unwrap().clone();
    params.rollover = Rollover { tz: chrono_tz::America::New_York, hour: 9, minute: 30 };
    h.operator_admin(AdminCommand::SetParams(params));
    let mut spec = h.engine.markets()[&AAPL].spec.clone();
    spec.financing.markup_bps = 3650;
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    accepted(&schedule(&mut h, dividend("same-instant", AAPL, "1", "0")));
    let events = tick(&mut h, open());
    let dividend = events.iter().position(|e| matches!(e, Event::CorporateDividend { .. })).unwrap();
    let financing = events.iter().position(|e| matches!(e, Event::Financing { .. })).unwrap();
    assert!(dividend < financing, "{events:?}");
    assert_eq!(payment(&events, 1), f("2"));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn completed_and_failed_action_identities_survive_restart_without_reapplying() {
    let mut h = held("2", "0");
    accepted(&schedule(&mut h, split("done", 4, 1)));
    tick(&mut h, open());
    h.engine = restored(&h.engine);
    let mut repeat = split("done", 4, 1);
    repeat.previous_close = ny(2026, 10, 21, 16, 0);
    repeat.ex_open = ny(2026, 10, 22, 9, 30);
    rejected(&schedule(&mut h, repeat), "duplicate corporate action id");
    // A mark that a further four-for-one split would round to zero makes that split fail.
    h.advance_to(ny(2026, 10, 21, 10, 0));
    h.price(AAPL, "0.000000003");
    let position = h.engine.accounts()[&acct(1)].position(AAPL);
    let mut failing = split("failed", 4, 1);
    failing.previous_close = ny(2026, 10, 21, 16, 0);
    failing.ex_open = ny(2026, 10, 22, 9, 30);
    accepted(&schedule(&mut h, failing));
    let events = tick(&mut h, ny(2026, 10, 22, 9, 30));
    assert!(events.iter().any(|e| matches!(e, Event::CorporateActionFailed { .. })));
    h.engine = restored(&h.engine);
    let events = h.cmd(Command::Tick);
    assert!(!events.iter().any(|e| matches!(e, Event::CorporateActionFailed { .. } | Event::CorporateSplit { .. })));
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL), position);
    assert!(h.engine.markets()[&AAPL].corporate_action_blocked);
}

#[test]
fn rejected_benchmark_refresh_cannot_run_opening_fills_ahead_of_a_due_split() {
    let mut h = held("2", "0");
    accepted(&schedule(&mut h, split("pending", 4, 1)));
    tick(&mut h, close());
    h.order(acct(1), AAPL, Side::Buy, "1", engine_core::command::OrderKind::Limit { price: f("80") });
    h.advance_to(open());
    let events = h.operate(AdminCommand::SetBenchmarkRates(BenchmarkBatch {
        as_of: "1900-01-01".parse().unwrap(),
        checked: ["SOFR".into()].into_iter().collect(),
        rates: vec![],
    }));
    assert!(events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::Fill(_) | Event::CorporateSplit { .. })), "{events:?}");
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("2"));
    assert!(h.engine.corporate_actions().contains_key("pending"));
    // The next admitted command is the opening quote: it applies the pending split before its own
    // opening fill, which the rejected batch deferred.
    let events = h.price(AAPL, "20");
    let split = events.iter().position(|e| matches!(e, Event::CorporateSplit { .. })).unwrap();
    let fill = events.iter().position(|e| matches!(e, Event::Fill(_))).unwrap();
    assert!(split < fill);
    assert_eq!(h.engine.accounts()[&acct(1)].position(AAPL).qty, f("12"));
}

#[test]
fn direct_opening_tick_and_timestamp_jump_fill_the_same_resting_close_before_liquidation() {
    let mut h = held("0", "10");
    accepted(&schedule(&mut h, dividend("closing-order", AAPL, "1000", "0")));
    tick(&mut h, close());
    let events = h.order(acct(2), AAPL, Side::Buy, "10", engine_core::command::OrderKind::Limit { price: f("80") });
    let order = events
        .iter()
        .find_map(|e| match e {
            Event::OrderAccepted { order_id, .. } => Some(*order_id),
            _ => None,
        })
        .expect("closing order can rest while the venue is closed");
    let mut direct = restored(&h.engine);
    let mut jumped = restored(&h.engine);
    // Under V21 the opening itself carries no quotes (the closure dropped them), so the fill waits
    // for the first quote after it in both legs and lands at the same instant.
    let tick_at = |engine: &mut Engine| {
        let at = open();
        engine
            .apply(&engine_core::command::Sequenced { seq: engine.last_seq().unwrap() + 1, ts: at, cmd: Command::Tick })
            .unwrap()
    };
    let quote_at = |engine: &mut Engine, at| {
        let quotes = vec![quote("opening-a", "80", at), quote("opening-b", "80", at)];
        engine
            .apply(&engine_core::command::Sequenced {
                seq: engine.last_seq().unwrap() + 1,
                ts: at,
                cmd: Command::Prices { market: AAPL, quotes },
            })
            .unwrap()
    };
    // Direct: a tick at the open applies the dividend at the boundary; the next command's quote fills.
    let events = tick_at(&mut direct);
    assert!(events.iter().any(|e| matches!(e, Event::CorporateDividend { .. })), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    let events = quote_at(&mut direct, open() + 1);
    assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::Fill(fill) if fill.order_id == order)), "{events:?}");
    // Jump: one command crosses the open; the dividend applies at the intermediate boundary before
    // the quote's own fill, with the same outcome.
    let events = quote_at(&mut jumped, open() + 1);
    assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::Fill(fill) if fill.order_id == order)), "{events:?}");
    let adjustment = events.iter().position(|e| matches!(e, Event::CorporateDividend { .. })).unwrap();
    let fill = events.iter().position(|e| matches!(e, Event::Fill(_))).unwrap();
    assert!(adjustment < fill);
    for engine in [&direct, &jumped] {
        assert!(engine.accounts()[&acct(2)].positions.is_empty());
        assert_eq!(engine.accounts()[&acct(2)].balance, Fixed::ZERO);
    }
    let (a, b) = (&direct.accounts()[&acct(2)], &jumped.accounts()[&acct(2)]);
    assert_eq!((a.balance, &a.positions, &a.orders), (b.balance, &b.positions, &b.orders));
    assert_eq!(direct.house_cash(), jumped.house_cash());
    assert_eq!(serde_json::to_value(&direct).unwrap()["markouts"], serde_json::to_value(&jumped).unwrap()["markouts"]);
}

#[test]
fn protocol_activation_leaves_missed_rollovers_and_due_openings_to_the_next_command() {
    let mut h = held("2", "0");
    let mut spec = h.engine.markets()[&AAPL].spec.clone();
    spec.financing.markup_bps = 3650;
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    accepted(&schedule(&mut h, split("pending", 4, 1)));
    let rollover = h.engine.next_rollover();
    h.advance_to(open());
    // Under V23 the activation is refused as not newer; either way it runs no catch-up.
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V23 }));
    assert!(events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
    assert!(
        !events.iter().any(|e| matches!(e, Event::Financing { .. } | Event::CorporateSplit { .. } | Event::Fill(_))),
        "{events:?}"
    );
    assert_eq!(h.engine.next_rollover(), rollover);
    assert!(h.engine.corporate_actions().contains_key("pending"));
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|e| matches!(e, Event::Financing { .. })), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::CorporateSplit { .. })), "{events:?}");
    assert_ne!(h.engine.next_rollover(), rollover);
}
