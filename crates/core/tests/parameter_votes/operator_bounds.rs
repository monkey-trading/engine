//! Parameter votes held to `V76`'s bounds: a voted relisting loosens a market's price rules within its
//! launch rules and no further, a voted rollover time never brings the due rollover earlier, the
//! shorter day after it charged for the time it runs, and voted corporate bounds need no notice but
//! never loosen past launch's.

use super::*;
use engine_core::Fixed;
use engine_core::fixed::Rounding;
use engine_core::market::PriceRules;
use engine_core::params::Rollover;

const HOUR_MS: Millis = 60 * 60 * 1000;

/// `market`'s baseline listing with `price_rules`.
fn relisting(h: &Harness, market: MarketId, price_rules: PriceRules) -> AdminCommand {
    AdminCommand::UpsertMarket(MarketSpec { price_rules, ..h.engine.baseline_spec(market).expect("listed") })
}

/// Passes `command` as `proposal` and applies it seven days on, with BTC freshly priced.
fn voted(h: &mut Harness, proposal: u64, command: AdminCommand) -> Vec<Event> {
    assert!(matches!(pass(h, proposal, false, command.clone()).last(), Some(Event::ProposalScheduled { .. })));
    h.advance(NOTICE_MS);
    h.price(BTC, "10000");
    apply(h, proposal, command)
}

/// What account 1 paid in financing among `events`.
fn financing(events: &[Event]) -> Fixed {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Financing { account, amount, .. } if *account == acct(1) => Some(*amount),
            _ => None,
        })
        .sum()
}

#[test]
fn a_voted_relisting_loosens_price_rules_back_to_launch_and_no_further() {
    let mut h = governed_under(ProtocolVersion::V76, TTL);
    let launch = h.engine.markets()[&BTC].spec.price_rules;
    assert_eq!(h.engine.operator_bounds().launch_bands[&BTC].price_rules, Some(launch));
    let tighter = relisting(&h, BTC, PriceRules { max_age_ms: launch.max_age_ms / 2, ..launch });
    assert_eq!(voted(&mut h, 1, tighter).last(), Some(&Event::AdminApplied));
    let back = relisting(&h, BTC, launch);
    assert_eq!(voted(&mut h, 2, back).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&BTC].spec.price_rules, launch);
    let past = relisting(&h, BTC, PriceRules { max_divergence_bps: launch.max_divergence_bps + 1, ..launch });
    rejected(&voted(&mut h, 3, past), "are looser than its launch rules");
    assert_eq!(h.engine.markets()[&BTC].spec.price_rules, launch);
}

#[test]
fn a_voted_rollover_time_never_brings_the_due_rollover_earlier() {
    let mut h = governed_under(ProtocolVersion::V76, TTL);
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 430 });
    let rollover = h.engine.params().expect("set").rollover.clone();
    let earlier = Rollover { hour: rollover.hour - 1, ..rollover.clone() };
    let command = with_params(&h, |params| params.rollover = earlier.clone());
    assert!(matches!(pass(&mut h, 1, false, command.clone()).last(), Some(Event::ProposalScheduled { .. })));
    // The change applies an hour and a half before a rollover at least seven days on.
    let mut due = rollover.next_after(h.ts + NOTICE_MS);
    if due - 90 * 60_000 < h.ts + NOTICE_MS {
        due = rollover.next_after(due);
    }
    h.advance_to(due - 90 * 60_000);
    h.price(BTC, "10000");
    assert_eq!(apply(&mut h, 1, command).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.params().expect("set").rollover, earlier);
    assert_eq!(h.engine.next_rollover(), Some(due), "the due rollover stays due");

    h.advance_to(due);
    let one_day = financing(&h.price(BTC, "10000"));
    assert!(one_day.is_positive(), "the long pays financing");
    let next = h.engine.next_rollover().expect("scheduled");
    assert_eq!(next, earlier.next_after(due));
    assert!(next - due < 24 * HOUR_MS);
    h.advance_to(next);
    let short_day = financing(&h.price(BTC, "10000"));
    let span = Fixed::from_int(next - due);
    assert_eq!(short_day, one_day.mul_div(span, Fixed::from_int(24 * HOUR_MS), Rounding::Up));
}

#[test]
fn a_voted_corporate_bounds_change_needs_no_notice_and_never_loosens_past_launch() {
    let mut h = governed_under(ProtocolVersion::V76, TTL);
    let looser = with_params(&h, |params| params.corporate_bounds.max_dividend_bps = 1_001);
    rejected(&voted(&mut h, 1, looser), "looser than launch's");
    let tighter = with_params(&h, |params| params.corporate_bounds.min_trading_days = 3);
    assert_eq!(voted(&mut h, 2, tighter).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.params().expect("set").corporate_bounds.min_trading_days, 3);
}
