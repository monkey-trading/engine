//! The mass events that close out holders and change stress: the weekend gap's start, a crash and a
//! stress level change.

use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::market::QuoteCurrency;
use engine_core::types::MarketId;

use crate::common::BTC;
use crate::common::f;
use crate::common::launch_book::{Mix, NamedCounts, WEEKEND_MARGINS, Weak, WeakKind, build, price_command};
use crate::report::timed;
use crate::support::{accounts, assert_financed, count, finish, positions_held, report, roll_to};

const NAMED: NamedCounts = NamedCounts { adders: 1, openers: 0, closeouts: 0 };
const WTI: MarketId = 32;
/// The price BTC-USD falls to, a tenth below the book's mark.
const CRASH_MID: &str = "90";

fn close_outs(events: &[Event]) -> usize {
    count(events, |e| matches!(e, Event::CloseOut { .. }))
}

/// The start of WTI's weekend gap, at `WEEKEND_MARGINS`, when every account holding WTI whose equity is below
/// its margin requirement with the weekend margin is closed out on it, with `share` per cent of its holders
/// below it.
/// The Friday rollover falls at the same instant, so the case with none below it is the baseline.
fn weekend(share: u64) {
    let mix = Mix {
        weak: Some(Weak { market: WTI, share, kind: WeakKind::BelowWeekendMargin }),
        weekend_margins: WEEKEND_MARGINS.to_vec(),
        ..Mix::proposal(accounts())
    };
    let mut book = build(&mix, NAMED);
    let holders = book.holders[&WTI];
    let gap = book.h.engine.markets()[&WTI].weekend_window().expect("WTI has a weekend gap");
    roll_to(&mut book, gap.close);
    assert_eq!(book.h.engine.next_rollover(), Some(gap.close), "the Friday rollover falls at the gap's start");
    let held = positions_held(&book);
    let (events, us) = timed(|| book.apply(Command::Tick));
    let closed = close_outs(&events);
    let extra = format!("holders={holders} below_margin={} closed_out={closed}", book.weak_accounts);
    assert_financed(&events, held, count(&events, |e| matches!(e, Event::Fill(_))));
    assert_eq!(closed, book.weak_accounts, "the accounts below the weekend margin, and no others, are closed out");
    report(&format!("mass_weekend_gap_start_{share}pct"), us, &events, &extra);
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_weekend_gap_start() {
    weekend(0);
    weekend(20);
    finish();
}

/// One price of BTC-USD a tenth down that closes out `share` per cent of the accounts holding it, and
/// also fills the book's in-band resting buys it reaches.
fn crash(share: u64) {
    let mix = Mix {
        weak: Some(Weak { market: BTC, share, kind: WeakKind::FallsBelowAt(f(CRASH_MID)) }),
        ..Mix::proposal(accounts())
    };
    let mut book = build(&mix, NAMED);
    let holders = book.holders[&BTC];
    book.h.advance(70_000);
    let ids: Vec<MarketId> = book.h.engine.markets().keys().copied().filter(|id| *id != BTC).collect();
    for id in ids {
        let cmd = price_command(&book.h, id, "100", book.h.ts);
        let events = book.apply(cmd);
        assert_eq!(close_outs(&events), 0, "market {id}: {events:?}");
    }
    let cmd = price_command(&book.h, BTC, CRASH_MID, book.h.ts);
    let (events, us) = timed(|| book.apply(cmd));
    let closed = close_outs(&events);
    let extra = format!("holders={holders} below_level={} closed_out={closed}", book.weak_accounts);
    assert_eq!(closed, book.weak_accounts, "the accounts below the close-out level, and no others, close");
    report(&format!("mass_crash_closes_out_{share}pct"), us, &events, &extra);
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_crash() {
    crash(20);
    crash(50);
    finish();
}

/// One price that puts the house behind its clients on the day, which raises every market to level 2
/// through V62's loss trigger (the production genesis's level 2 threshold is zero).
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_stress_level_change() {
    let mut book = build(&Mix::proposal(accounts()), NAMED);
    let markets = book.h.engine.markets().len();
    // The USD-quoted market with the most net client quantity, moved five per cent the way its clients
    // gain. The in-band orders the move reaches fill, as they would at launch, and their spreads are
    // far less than the gain.
    let (market, mid) = book
        .h
        .engine
        .markets()
        .iter()
        .filter(|(_, market)| matches!(market.spec.quote_currency, QuoteCurrency::Usd))
        .map(|(id, market)| (*id, market.client_qty))
        .max_by_key(|(_, qty)| qty.abs())
        .map(|(id, qty)| (id, if qty.is_positive() { "105" } else { "95" }))
        .expect("a USD market");
    book.h.advance(70_000);
    let ids: Vec<MarketId> = book.h.engine.markets().keys().copied().filter(|id| *id != market).collect();
    for id in ids {
        let cmd = price_command(&book.h, id, "100", book.h.ts);
        book.apply(cmd);
    }
    let cmd = price_command(&book.h, market, mid, book.h.ts);
    let (events, us) = timed(|| book.apply(cmd));
    let raised = count(&events, |e| matches!(e, Event::StressChanged { level: 2, .. }));
    let loss = events.iter().any(|e| matches!(e, Event::HouseLossChanged { level: 2, .. }));
    let extra =
        format!("market={market} mid={mid} markets={markets} raised_to_level_2={raised} house_loss_level_2={loss}");
    assert!(loss, "the price puts the house behind: {extra} loss={:?}", book.h.engine.house_loss());
    assert_eq!(raised, markets, "every market rises to level 2: {extra}");
    report("mass_stress_change_0_to_2", us, &events, &extra);
    finish();
}
