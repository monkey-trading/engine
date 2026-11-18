//! `V80`'s rollover (#933) against the rule before it, on twin launch books that differ only in their
//! version: at every rollover, the weekend gap's and a catch-up of two included, the rows `V80`
//! derives are the `Financing` events the earlier rule logs, in order and amount; every other event is
//! the same; each `FinancingApplied` counts its rows' positions and accounts, sums their net and
//! roots them; and every balance and house cash end equal.

mod common;

use std::collections::BTreeSet;

use common::launch_book::{Book, Mix, NamedCounts, benchmark_batch, build, price_command};
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::financing::{ChargesTree, FinancingRow};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::MarketId;

const ACCOUNTS: usize = 2_000;
const NAMED: NamedCounts = NamedCounts { adders: 1, openers: 0, closeouts: 0 };
/// The rule before `V80`, which logs a row per position.
const BEFORE: ProtocolVersion = ProtocolVersion::V79;
/// Rollovers walked; the two after `MISSED` are caught up by one command.
const ROLLOVERS: usize = 8;
const MISSED: usize = 4;

#[test]
fn every_rollover_derives_the_rows_the_earlier_rule_logs_and_moves_cash_alike() {
    assert!(ProtocolVersion::LATEST >= ProtocolVersion::V80);
    let mut books =
        [BEFORE, ProtocolVersion::LATEST].map(|version| build(&Mix { version, ..Mix::proposal(ACCOUNTS) }, NAMED));
    let (mut rows_seen, mut aggregates_seen, mut catch_up_seen) = (0, 0, false);
    let mut step = 0;
    while step < ROLLOVERS {
        // Two rollovers due before one command, once.
        let catch_up = step == MISSED;
        let [(logged, _), (events, derived)] = books.each_mut().map(|book| {
            let due = book.h.engine.next_rollover().expect("a rollover is due");
            publish(book, due);
            reprice(book, due - 1);
            // The rollover after a missed one charges at the rate dated before it.
            let until = if catch_up { book.h.engine.params().unwrap().rollover.next_after(due) } else { due };
            book.h.advance_to(until);
            let events = book.apply(Command::Tick);
            (events, book.h.engine.take_derived())
        });
        let rows: Vec<Event> = logged.iter().filter(|e| matches!(e, Event::Financing { .. })).cloned().collect();
        assert!(!rows.is_empty(), "rollover {step} finances positions");
        let derived_events: Vec<Event> = derived.iter().map(FinancingRow::event).collect();
        assert_eq!(derived_events, rows, "rollover {step}: the derived rows are the rows the earlier rule logs");
        let others = |events: &[Event], kind: fn(&Event) -> bool| -> Vec<Event> {
            events.iter().filter(|e| !kind(e)).cloned().collect()
        };
        assert_eq!(
            others(&logged, |e| matches!(e, Event::Financing { .. })),
            others(&events, |e| matches!(e, Event::FinancingApplied { .. })),
            "rollover {step}: every other event is the same"
        );
        let mut rest = &derived[..];
        for event in &events {
            let Event::FinancingApplied { positions, accounts, net, root, .. } = event else { continue };
            let (own, after) = rest.split_at(usize::try_from(*positions).unwrap());
            rest = after;
            let charged: BTreeSet<_> = own.iter().map(|row| row.account).collect();
            assert_eq!(charged.len() as u64, *accounts);
            assert_eq!(own.iter().map(|row| row.amount).sum::<Fixed>(), *net);
            assert_eq!(ChargesTree::new(own).root(), *root);
            aggregates_seen += 1;
        }
        assert!(rest.is_empty(), "rollover {step}: the aggregates cover every row");
        catch_up_seen |= catch_up && events.iter().filter(|e| matches!(e, Event::FinancingApplied { .. })).count() == 2;
        let [before, after] = &books;
        assert_eq!(before.h.engine.house_cash(), after.h.engine.house_cash(), "rollover {step}: house cash");
        let balance =
            |book: &Book| book.h.engine.accounts().iter().map(|(a, acct)| (*a, acct.balance)).collect::<Vec<_>>();
        assert_eq!(balance(before), balance(after), "rollover {step}: every balance");
        rows_seen += rows.len();
        step += if catch_up { 2 } else { 1 };
    }
    assert!(catch_up_seen, "one command caught two rollovers up, an aggregate each");
    assert!(
        aggregates_seen >= ROLLOVERS && rows_seen > ACCOUNTS * ROLLOVERS,
        "{aggregates_seen} rollovers, {rows_seen} rows"
    );
}

// ── Helpers ──

/// Publishes the benchmark rates a millisecond before the rollover at `due`.
fn publish(book: &mut Book, due: i64) {
    book.h.advance_to(due - 1);
    let batch = benchmark_batch(&book.h);
    let events = book.operate(batch);
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
}

/// Quotes every market at its own mark at `at`, as a node's feeds keep the open markets priced.
fn reprice(book: &mut Book, at: i64) {
    let marks: Vec<(MarketId, Fixed)> =
        book.h.engine.markets().iter().filter_map(|(id, m)| m.mark.map(|mark| (*id, mark))).collect();
    for (id, mark) in marks {
        let cmd = price_command(&book.h, id, &mark.to_string(), at);
        book.apply(cmd);
    }
}
