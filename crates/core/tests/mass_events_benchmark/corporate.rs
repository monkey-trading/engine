//! A 2:1 split and a cash dividend on a share market that half the accounts hold: the capture of each
//! holder's entitlement at the market's close, and the adjustment at its next open.

use engine_core::command::{AdminCommand, Command};
use engine_core::corporate::{Adjustment, CorporateAction};
use engine_core::event::Event;
use engine_core::types::{MarketId, Millis};

use crate::common::launch_book::{Mix, NamedCounts, build};
use crate::common::{f, ny};
use crate::report::timed;
use crate::support::{accounts, finish, held, report, roll_to};

const NAMED: NamedCounts = NamedCounts { adders: 1, openers: 0, closeouts: 0 };
/// AAPL, which half the accounts hold in this book.
const AAPL: MarketId = 40;

/// Schedules `adjustment` on AAPL for `previous_close` and `ex_open`, then times the command at each. From
/// `V76` an action leaves two trading days after it is scheduled, and a split a notice of seven days.
fn corporate(case: &str, adjustment: Adjustment, previous_close: Millis, ex_open: Millis) {
    let mix = Mix { also_hold: vec![(AAPL, 50)], ..Mix::proposal(accounts()) };
    let mut book = build(&mix, NAMED);
    let holders = book.holders[&AAPL];
    let action =
        CorporateAction { id: case.into(), market: AAPL, previous_close, ex_open, adjustment: adjustment.clone() };
    let events = book.operate(AdminCommand::ScheduleCorporateAction(action.clone()));
    assert!(events.iter().any(|e| matches!(e, Event::CorporateActionScheduled { .. })), "{events:?}");
    roll_to(&mut book, action.previous_close);
    let (events, us) = timed(|| book.apply(Command::Tick));
    let captured: usize = events
        .iter()
        .filter_map(
            |e| if let Event::CorporateEntitlementsCaptured { accounts, .. } = e { Some(*accounts) } else { None },
        )
        .sum();
    assert_eq!(captured, holders, "every holder's entitlement is captured");
    report(&format!("mass_{case}_capture"), us, &events, &format!("holders={holders} captured={captured}"));
    roll_to(&mut book, action.ex_open);
    let before: Vec<_> = book
        .h
        .engine
        .accounts()
        .iter()
        .filter_map(|(id, account)| Some((*id, account.positions.get(&AAPL)?.qty)))
        .collect();
    assert_eq!(before.len(), holders);
    let (events, us) = timed(|| book.apply(Command::Tick));
    match adjustment {
        Adjustment::Split { .. } => {
            assert!(events.iter().any(|e| matches!(e, Event::CorporateSplit { .. })), "{}", events.len());
            for (id, qty) in &before {
                assert_eq!(
                    book.h.engine.accounts()[id].positions[&AAPL].qty,
                    qty.mul_int(2),
                    "{id:?}'s position doubles"
                );
            }
        }
        Adjustment::Dividend { .. } => {
            let paid = events.iter().filter(|e| matches!(e, Event::CorporateDividend { .. })).count();
            assert_eq!(paid, captured, "every holder is paid");
        }
    }
    assert!(!events.iter().any(|e| matches!(e, Event::CorporateActionFailed { .. })), "{events:?}");
    report(&format!("mass_{case}_ex_open"), us, &events, &format!("holders={holders}"));
    finish();
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_split_two_for_one() {
    held("mass_split_two_for_one", mass_split_two_for_one_measured);
}

fn mass_split_two_for_one_measured() {
    corporate(
        "split",
        Adjustment::Split { new_units: 2, old_units: 1 },
        ny(2026, 11, 24, 16, 0),
        ny(2026, 11, 25, 9, 30),
    );
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_cash_dividend() {
    held("mass_cash_dividend", mass_cash_dividend_measured);
}

fn mass_cash_dividend_measured() {
    let dividend = Adjustment::Dividend { gross_per_unit: f("0.25"), withholding_rate: f("0.15") };
    corporate("dividend", dividend, ny(2026, 11, 18, 16, 0), ny(2026, 11, 19, 9, 30));
}
