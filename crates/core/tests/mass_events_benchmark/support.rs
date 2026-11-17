//! What the mass cases share: the book's size, the bar and its guard, the clock's walk to an event, and
//! the tally of what it emitted.

use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::types::Millis;

use crate::common::launch_book::{Book, benchmark_batch};
use crate::report::noted;

/// The proposal's book size; `MASS_ACCOUNTS` runs a smaller one.
pub fn accounts() -> usize {
    std::env::var("MASS_ACCOUNTS").ok().map_or(100_000, |n| n.parse().expect("a count"))
}

/// #904's bar for any single command, the largest budget today (microseconds), and the guard CI holds a
/// mass event to where the book is known to miss it, six times the bar.
pub const BAR_US: u128 = 500_000;
pub const GUARD_US: u128 = 3_000_000;

thread_local! {
    /// The cases over the guard so far in this test, which `finish` fails it for once every case has
    /// printed. Each test runs on its own thread, so one's excess is never another's.
    static OVER_GUARD: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Prints `name`'s time and what it emitted, its standing against the bar, and notes it if over the guard.
pub fn report(name: &str, us: u128, events: &[Event], extra: &str) {
    println!("MASS {name} us={us} events={} {} {extra}", events.len(), tally(events));
    noted(name, us, BAR_US);
    if us > GUARD_US {
        OVER_GUARD.with_borrow_mut(|over| over.push(format!("{name} took {us}us against {GUARD_US}us")));
    }
}

/// Ends a test: fails it if any case it ran was over the guard. Drains the notes, so one test's excess
/// fails only that test.
pub fn finish() {
    let over = OVER_GUARD.with_borrow_mut(std::mem::take);
    assert!(over.is_empty(), "over the guard: {over:?}");
}

/// As `report`, with no guard: the owner accepted this case over the bar (#904).
pub fn report_accepted(name: &str, us: u128, events: &[Event], extra: &str) {
    println!("MASS {name} us={us} events={} {} {extra}", events.len(), tally(events));
    noted(name, us, BAR_US);
}

/// The counts of the events a mass case emits, in the order the issue lists them.
pub fn tally(events: &[Event]) -> String {
    let count = |matches: fn(&Event) -> bool| events.iter().filter(|e| matches(e)).count();
    format!(
        "close_outs={} fills={} cancelled={} financing={} stress_changes={} dividends={} cash_in_lieu={} \
         emissions={} distributions={}",
        count(|e| matches!(e, Event::CloseOut { .. })),
        count(|e| matches!(e, Event::Fill(_))),
        count(|e| matches!(e, Event::OrderCancelled { .. })),
        count(|e| matches!(e, Event::Financing { .. })),
        count(|e| matches!(e, Event::StressChanged { .. })),
        count(|e| matches!(e, Event::CorporateDividend { .. })),
        count(|e| matches!(e, Event::CorporateCashInLieu { .. })),
        count(|e| matches!(e, Event::EmissionAllocated { .. })),
        count(|e| matches!(e, Event::DistributionAllocated { .. })),
    )
}

/// How many events of `matches` `events` hold.
pub fn count(events: &[Event], matches: fn(&Event) -> bool) -> usize {
    events.iter().filter(|e| matches(e)).count()
}

/// Walks the book's clock to `to` through the daily rollovers before it, each with its benchmark rates
/// published first, as the rollover case does, so the command at `to` is the first to find anything
/// due. A rollover at `to` itself is left for that command, its rates already published.
pub fn roll_to(book: &mut Book, to: Millis) {
    while let Some(due) = book.h.engine.next_rollover() {
        if due > to {
            break;
        }
        book.h.advance_to(due - 1);
        let batch = benchmark_batch(&book.h);
        let events = book.operate(batch);
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        if due == to {
            break;
        }
        book.h.advance_to(due);
        book.apply(Command::Tick);
    }
    book.h.advance_to(to);
}

/// Checks a rollover's `Financing` events against the positions held before it: one for each, or one for
/// each but those the command's own close-outs closed first (`closed`, one fill each).
pub fn assert_financed(events: &[Event], held: usize, closed: usize) {
    let financed = count(events, |e| matches!(e, Event::Financing { .. }));
    assert!(financed == held || financed + closed == held, "{financed} financed, {closed} closed, of {held}");
}

/// The positions the book holds now.
pub fn positions_held(book: &crate::common::launch_book::Book) -> usize {
    book.h.engine.accounts().values().map(|account| account.positions.len()).sum()
}
