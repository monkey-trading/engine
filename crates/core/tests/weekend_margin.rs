//! `V59`'s weekend margin, through `Engine::apply`: from two hours before a market's weekend gap
//! starts, an open or add in it meets the weekend margin; when the gap starts, an account below it is
//! closed out at the market's last mark; once the market reopens, the normal margin applies again;
//! a holiday that starts the gap early moves both; and a market with no weekend margin keeps its
//! normal margin throughout. What the margin holds over the gap is in `weekend_margin_gap.rs`, and the
//! operator's bounds on the rate in `weekend_margin_bounds.rs`.

mod common;

use common::close_spread::{reduce, trade};
use common::events::{fills, only_fill, rejection};
use common::weekend::{GOLD, OIL, OIL_WEEKEND_BPS, chicago, client, genesis_v59, globex, period};
use common::{Harness, f};
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::event::{Event, Reject};
use engine_core::types::{Millis, Side};
use engine_core::weekend::{WEEKEND_LEAD_MS, WeekendGap, WeekendRequirement, WeekendWindow};

/// Friday 23 October 2026's 16:00 Globex close in Chicago, which starts that weekend's gap.
fn friday_close() -> Millis {
    chicago(2026, 10, 23, 16, 0)
}

/// Sunday 25 October 2026's 17:00 Globex open in Chicago, which ends it.
fn sunday_open() -> Millis {
    chicago(2026, 10, 25, 17, 0)
}

#[test]
fn an_open_or_add_from_two_hours_before_the_gap_meets_the_weekend_margin() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "10000");
    h.price(OIL, "70");
    // Mid-week, $700k at the 1% band.
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    let close = friday_close();
    let (from, reopen) = (close - WEEKEND_LEAD_MS, sunday_open());
    let window = WeekendWindow { weekend_margin_bps: OIL_WEEKEND_BPS, from, close, reopen };
    let requirement = WeekendRequirement { window, required: f("52500") };
    assert_eq!(h.engine.weekend_requirements(a.account).get(&OIL), Some(&requirement));

    // A millisecond before the lead, an add takes the normal margin.
    h.advance_to(close - WEEKEND_LEAD_MS - 1);
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "100", "70"));
    // From the lead, the account must hold 7.5% of the $714k it would hold, at the fill's index.
    h.advance(1);
    h.price(OIL, "70");
    let refused = rejection(&trade(&mut h, &a, OIL, Side::Buy, "100", "70")).clone();
    let Reject::WeekendMargin { equity, required } = refused else { panic!("{refused:?}") };
    assert_eq!(required, f("53550"));
    assert!(equity < required && equity >= f("7140"), "{equity} meets the normal margin");
    // A reduction fills, and an account that holds the weekend margin opens.
    only_fill(&reduce(&mut h, &a, OIL, Side::Sell, "100", "70"));
    let b = client(&mut h, 2, "60000");
    only_fill(&trade(&mut h, &b, OIL, Side::Buy, "10000", "70"));
    // The lead runs to the close's last millisecond.
    h.advance_to(close - 1);
    h.price(OIL, "70");
    assert!(matches!(rejection(&trade(&mut h, &a, OIL, Side::Buy, "100", "70")), Reject::WeekendMargin { .. }));
}

#[test]
fn when_the_gap_starts_an_account_below_the_weekend_margin_is_closed_out_at_the_last_mark() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let (a, b) = (client(&mut h, 1, "10000"), client(&mut h, 2, "60000"));
    h.price(OIL, "70");
    for holder in [&a, &b] {
        only_fill(&trade(&mut h, holder, OIL, Side::Buy, "10000", "70"));
    }
    let close = friday_close();
    h.advance_to(close - 500);
    h.price(OIL, "70.5");
    // The first command after the close is on Saturday.
    h.advance_to(close + 20 * HOUR_MS);
    let events = h.cmd(Command::Tick);
    let closed = only_fill(&events).clone();
    assert!(closed.liquidation && closed.account == a.account && closed.market == OIL, "{closed:?}");
    assert_eq!(
        (closed.side, closed.qty, closed.index, closed.position),
        (Side::Sell, f("10000"), f("70.5"), Fixed::ZERO)
    );
    let close_out =
        events.iter().position(|event| matches!(event, Event::CloseOut { account, .. } if *account == a.account));
    let Some(Event::CloseOut { equity, margin, .. }) = close_out.map(|at| &events[at]) else { panic!("{events:?}") };
    assert_eq!(*margin, f("52875"), "7.5% of $705k at the last mark");
    assert!(*equity < *margin);
    // The close-out comes before the rollover at the same instant, which finances only `b`.
    let financed: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(at, event)| match event {
            Event::Financing { account, .. } => Some((at, *account)),
            _ => None,
        })
        .collect();
    assert_eq!(financed.iter().map(|(_, account)| *account).collect::<Vec<_>>(), [b.account]);
    assert!(financed.iter().all(|(at, _)| *at > close_out.unwrap()), "{events:?}");
    assert_eq!(position(&h, &b), Some(f("10000")));
    // The gap holds until Sunday's open, and then the next starts a week on.
    let reopen = sunday_open();
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(WeekendGap { close, reopen, closed_out: true }));
    h.advance_to(reopen);
    h.cmd(Command::Tick);
    // Daylight saving ends on 1 November, so the next Sunday open is an hour later in UTC.
    let next = WeekendGap { close: close + WEEK_MS, reopen: chicago(2026, 11, 1, 17, 0), closed_out: false };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(next));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn once_the_market_reopens_the_normal_margin_applies_again() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "10000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    h.advance_to(friday_close() + HOUR_MS);
    only_fill(&h.cmd(Command::Tick));
    // Sunday's 17:00 open: the same $700k takes the 1% band again.
    h.advance_to(sunday_open());
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    assert_eq!(position(&h, &a), Some(f("10000")));
}

#[test]
fn a_market_with_no_weekend_margin_keeps_its_normal_margin_over_its_gap() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "10000");
    // Gold shuts with oil, but an open in its last hour takes the 1% band, and nothing closes it.
    h.advance_to(friday_close() - HOUR_MS);
    h.price(GOLD, "2600");
    only_fill(&trade(&mut h, &a, GOLD, Side::Buy, "269", "2600"));
    assert_eq!(h.engine.markets()[&GOLD].weekend, None);
    h.advance_to(friday_close() + HOUR_MS);
    assert!(fills(&h.cmd(Command::Tick)).is_empty());
    assert_eq!(position_in(&h, &a, GOLD), Some(f("269")));
}

#[test]
fn a_holiday_that_starts_the_gap_early_moves_the_lead_and_the_close_out() {
    // Friday's session ends at noon.
    let mut h = genesis_v59(globex(vec![period("2026-10-23 12:00", "2026-10-23 16:00")]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "10000");
    let noon = chicago(2026, 10, 23, 12, 0);
    assert_eq!(h.engine.markets()[&OIL].weekend.map(|gap| gap.close), Some(noon));
    h.advance_to(noon - WEEKEND_LEAD_MS - 1);
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    h.advance(1);
    h.price(OIL, "70");
    assert!(matches!(rejection(&trade(&mut h, &a, OIL, Side::Buy, "100", "70")), Reject::WeekendMargin { .. }));
    h.advance_to(noon + HOUR_MS);
    let closed = only_fill(&h.cmd(Command::Tick)).clone();
    assert!(closed.liquidation && closed.index == f("70"), "{closed:?}");
    assert_eq!(position(&h, &a), None);
}

#[test]
fn markets_whose_gaps_start_together_close_the_largest_loser_first_until_the_account_meets_them() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, OIL_WEEKEND_BPS);
    let a = client(&mut h, 1, "60000");
    h.price(OIL, "70");
    h.price(GOLD, "2600");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    only_fill(&trade(&mut h, &a, GOLD, Side::Buy, "100", "2600"));
    // Oil loses $10k and gold gains $1k: about $50.8k of equity against $71.3k at 7.5%. Closing oil
    // leaves gold's $19.6k, which the account meets.
    h.advance_to(friday_close() - 500);
    h.price(OIL, "69");
    h.price(GOLD, "2610");
    h.advance_to(friday_close() + HOUR_MS);
    let closed = only_fill(&h.cmd(Command::Tick)).clone();
    assert_eq!((closed.market, closed.index), (OIL, f("69")));
    assert_eq!(position_in(&h, &a, GOLD), Some(f("100")));
}

const HOUR_MS: Millis = 60 * 60 * 1000;
const WEEK_MS: Millis = 7 * 24 * HOUR_MS;

/// The client's oil position, if it holds one.
fn position(h: &Harness, client: &common::Client) -> Option<Fixed> {
    position_in(h, client, OIL)
}

/// The client's position in `market`, if it holds one.
fn position_in(h: &Harness, client: &common::Client, market: engine_core::types::MarketId) -> Option<Fixed> {
    h.engine.accounts()[&client.account].positions.get(&market).map(|pos| pos.qty)
}
