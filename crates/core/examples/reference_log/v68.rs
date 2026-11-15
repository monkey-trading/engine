//! The session's `V68` part: tier benefits. A vote activates the version, which gives the table's tiers
//! fee and financing discounts of 0, 0, 10% and 20% and emissions boosts of 0, 10%, 25% and 50%. A
//! wallet earns tier 1 over two days of counted volume and loses in an emission period, whose close
//! allocates it a weight 1.1 times its loss; a voted table that raises a discount above launch's, or
//! lowers tier 3's fee discount below tier 2's, is refused. Tier 2's discounts need $25M over five days
//! and the session's base profit fee is zero, so the fee and financing discounts are pinned by
//! `tests/tier_benefits.rs`.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::fixed::Rounding;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};
use engine_core::volume_tier::{
    ACTIVATION_EMISSIONS_BOOSTS_BPS, ACTIVATION_FEE_DISCOUNTS_BPS, ACTIVATION_FINANCING_DISCOUNTS_BPS, DAY_MS,
    MIN_HOLD_MS, day_of,
};

use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::only_fill;
use crate::common::{Client, EURUSD, Harness};
use crate::v56::pass;

/// The seed of the wallet this part trades through.
const CLIENT_SEED: u8 = 0x7c;

/// `V68` activates by vote. A fresh wallet buys $550k of EUR-USD and sells it a day later, each held 60 s
/// and counted, which reaches tier 1; a loss of about $1,000 at tier 1 is weighed 1.1 times when its
/// period closes. A voted table that raises tier 2's fee discount to 10.01%, and one that lowers tier 3's below
/// tier 2's, are refused.
pub(crate) fn session(h: &mut Harness) {
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V68 };
    scheduled(&pass(h, 100, false, Some(activate.clone())));
    h.advance(NOTICE_MS);
    applied(&apply(h, 100, activate));
    let tiers = h.engine.params().expect("set").volume_tiers.clone();
    assert_eq!(tiers.iter().map(|t| t.fee_discount_bps).collect::<Vec<_>>(), ACTIVATION_FEE_DISCOUNTS_BPS);
    assert_eq!(tiers.iter().map(|t| t.financing_discount_bps).collect::<Vec<_>>(), ACTIVATION_FINANCING_DISCOUNTS_BPS);
    assert_eq!(tiers.iter().map(|t| t.emissions_boost_bps).collect::<Vec<_>>(), ACTIVATION_EMISSIONS_BOOSTS_BPS);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let wallet = |h: &Harness| Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &wallet(h));
    fund(h, &wallet(h), "1000000");
    h.price(EURUSD, "1.1");
    only_fill(&trade(h, &wallet(h), EURUSD, Side::Buy, "500000", "1.1"));
    settle(h);
    let account = wallet(h).account;
    assert_eq!(h.engine.volume_tier(account), 0);
    next_weekday(h);
    h.price(EURUSD, "1.1");
    only_fill(&trade(h, &wallet(h), EURUSD, Side::Sell, "500000", "1.1"));
    settle(h);
    assert_eq!(h.engine.volume_tier(account), 1);

    // At tier 1 the wallet buys EUR 100,000 at 1.1 and sells it at 1.09: the period that holds the loss
    // allocates it 1.1 times the loss.
    next_weekday(h);
    h.price(EURUSD, "1.1");
    only_fill(&trade(h, &wallet(h), EURUSD, Side::Buy, "100000", "1.1"));
    h.price(EURUSD, "1.09");
    let sold = only_fill(&trade(h, &wallet(h), EURUSD, Side::Sell, "100000", "1.09")).clone();
    let loss = -sold.realised_pnl;
    assert!(loss.is_positive(), "{sold:?}");
    let period = h.engine.emissions().expect("initialised").period;
    let end = h.engine.emissions().expect("initialised").schedule.period_end(period);
    h.advance_to(end);
    let events = h.cmd(Command::Tick);
    let allocated = events.iter().find_map(|event| match event {
        Event::EmissionAllocated { account: got, weight, .. } if *got == account => Some(*weight),
        _ => None,
    });
    assert_eq!(allocated, Some(loss.mul_bps_rounded(11_000, Rounding::TowardZero)), "{events:?}");

    let mut generous = h.engine.baseline_params().expect("set");
    generous.volume_tiers[2].fee_discount_bps = 1_001;
    let mut falling = h.engine.baseline_params().expect("set");
    falling.volume_tiers[3].fee_discount_bps = 500;
    let generous = AdminCommand::SetParams(generous);
    let falling = AdminCommand::SetParams(falling);
    scheduled(&pass(h, 101, false, Some(generous.clone())));
    scheduled(&pass(h, 102, false, Some(falling.clone())));
    h.advance(NOTICE_MS);
    let events = apply(h, 101, generous);
    assert!(
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("above its launch value 1000")),
        "{events:?}"
    );
    let events = apply(h, 102, falling);
    assert!(
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("never fall with the tier")),
        "{events:?}"
    );
}

/// Holds the wallet's fill 60 s and lets the toxicity horizon pass, so the volume counts.
fn settle(h: &mut Harness) {
    h.advance(MIN_HOLD_MS);
    h.price(EURUSD, "1.1");
}

/// Moves to noon UTC of the next weekday after the current one, a day on, when FX trades.
fn next_weekday(h: &mut Harness) {
    let mut day = day_of(h.ts) + 1;
    // The epoch's day 0 is a Thursday; weekdays are Monday to Thursday here, clear of the weekend gap.
    while !matches!((day + 3).rem_euclid(7), 0..=3) {
        day += 1;
    }
    let at: Millis = day * DAY_MS + 12 * 60 * 60 * 1_000;
    h.advance_to(at);
}

/// Sequences the node's `ApplyProposal` for `proposal` with `command`.
fn apply(h: &mut Harness, proposal: u64, command: AdminCommand) -> Vec<Event> {
    h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command })))
}

fn scheduled(events: &[Event]) {
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
}

fn applied(events: &[Event]) {
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}
