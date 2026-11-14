//! The session's `V59` part: a market's weekend margin, which opens and adds meet from two hours
//! before its weekend gap starts, which closes out an account below it when the gap starts, and which
//! moves only by a passed vote while rotation is on.

use chrono::{DateTime, Datelike, Days, Utc};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Reject};
use engine_core::governance::VotedChange;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};
use engine_core::weekend::WEEKEND_LEAD_MS;

use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::{only_fill, rejection};
use crate::common::{AAPL, Client, Harness, ny};
use crate::v56::{TURN_SEED, pass};

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x66;
/// The weekend margin AAPL takes: 30%, above its 20% band.
const WEEKEND_BPS: i64 = 3_000;

/// Sequences the node's `ApplyProposal` for `proposal` with `command`.
fn apply(h: &mut Harness, proposal: u64, command: AdminCommand) -> Vec<Event> {
    h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command })))
}

fn refused(events: &[Event]) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
}

fn applied(events: &[Event]) {
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}

/// AAPL's baseline listing with a weekend margin.
fn weekend_aapl(h: &Harness) -> AdminCommand {
    let spec = h.engine.baseline_spec(AAPL).expect("listed");
    AdminCommand::UpsertMarket(MarketSpec { weekend_margin_bps: WEEKEND_BPS, ..spec })
}

/// With rotation on, the turn's key's notice of a weekend margin is refused, and a voted relisting
/// gives AAPL one with no listing notice; applied before `V59`, which a vote activates after it, it
/// does nothing until the activation starts it. A client's AAPL long opened three hours before the
/// weekend close cannot add in the last two, and is closed out at its last mark when the gap starts;
/// on Monday's open the normal margin applies again.
pub(crate) fn session(h: &mut Harness) {
    let weekend = weekend_aapl(h);
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 };
    for (proposal, command) in [(14, &weekend), (15, &activate)] {
        let events = pass(h, proposal, false, Some(command.clone()));
        assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    }
    let notice = ListingNotice {
        spread_tiers: None,
        margin_tiers: None,
        markup_bps: None,
        weekend_margin_bps: Some(WEEKEND_BPS),
        effective_at: h.ts + NOTICE_MS,
    };
    refused(&h.operate_as(TURN_SEED, AdminCommand::AnnounceListing { market: AAPL, notice }));
    h.advance(NOTICE_MS);
    applied(&apply(h, 14, weekend));
    assert_eq!(h.engine.markets()[&AAPL].weekend, None, "inert before V59");
    applied(&apply(h, 15, activate));

    let close = h.engine.markets()[&AAPL].weekend.expect("a weekend margin keeps its next gap").close;
    let client = Client::new(CLIENT_SEED);
    h.advance_to(close - 3 * HOUR_MS);
    h.cmd(Command::Tick);
    attest(h, &client);
    fund(h, &client, "10000");
    h.price(AAPL, "250");
    only_fill(&trade(h, &client, AAPL, Side::Buy, "190", "250"));
    h.advance_to(close - WEEKEND_LEAD_MS);
    h.price(AAPL, "250");
    assert!(matches!(rejection(&trade(h, &client, AAPL, Side::Buy, "1", "250")), Reject::WeekendMargin { .. }));
    h.advance_to(close - 1_000);
    h.price(AAPL, "251");
    h.advance_to(close + HOUR_MS);
    let closed = only_fill(&h.cmd(Command::Tick)).clone();
    assert!(closed.liquidation && closed.account == client.account, "{closed:?}");

    h.advance_to(next_monday_open(close));
    h.price(AAPL, "251");
    only_fill(&trade(h, &client, AAPL, Side::Buy, "10", "251"));
}

const HOUR_MS: Millis = 60 * 60 * 1000;

/// 09:31 in New York on the Monday after the Friday close `close`.
fn next_monday_open(close: Millis) -> Millis {
    let friday = DateTime::<Utc>::from_timestamp_millis(close).unwrap().with_timezone(&chrono_tz::America::New_York);
    let monday = friday.date_naive().checked_add_days(Days::new(3)).unwrap();
    ny(monday.year(), monday.month(), monday.day(), 9, 31)
}
