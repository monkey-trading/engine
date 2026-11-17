//! The session's `V76` part: the operator's bounds over the rollover, price rules and corporate actions,
//! while votes govern. A vote activates the version, which takes each market's price rules as its
//! launch rules, and the key in office's own corporate action is refused. Four proposals pass together
//! and apply seven days on, an hour and a half before a rollover: the rollover an hour earlier, with a
//! dividend cap of 8%, which leaves the due rollover due, so the one after it, 23 hours on, charges a
//! client's BTC long 23 hours of financing; a dividend of 15% of AAPL's mark, which the vote's notice lets past the 10% cap, two
//! of AAPL's sessions ahead; a 21-for-1 split, refused when it applies; and a relisting that lengthens
//! BTC's freshness bound past its launch rule, refused when it applies.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::corporate::{Adjustment, CorporateAction, CorporateBounds, LAUNCH_CORPORATE_BOUNDS};
use engine_core::event::Event;
use engine_core::fixed::Rounding;
use engine_core::governance::VotedChange;
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::params::{ProtocolParams, Rollover};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{Millis, Side};

use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::only_fill;
use crate::common::{AAPL, BTC, Client, Harness, f};
use crate::v56::pass;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x77;
const MINUTE_MS: Millis = 60_000;
const HOUR_MS: Millis = 60 * MINUTE_MS;

/// Activates `V76` by vote; then the bounds described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V76 };
    let events = pass(h, 190, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    applied(&apply(h, 190, activation));
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V76);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);
    let launch = h.engine.markets()[&BTC].spec.price_rules;
    assert_eq!(h.engine.operator_bounds().launch_bands[&BTC].price_rules, Some(launch));

    // A client long one BTC, held over the rollovers.
    let mark = h.engine.markets()[&BTC].mark.expect("BTC has traded").to_string();
    h.price(BTC, &mark);
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "1000000");
    only_fill(&trade(h, &client, BTC, Side::Buy, "1", &mark));

    // The key in office schedules no corporate action of its own while votes govern.
    let aapl = h.engine.markets()[&AAPL].spec.schedule.clone();
    let (close, open) = session_closes(&aapl, h.ts, 2);
    let own = dividend("own", Fixed::from_int(1), close, open);
    let events = h.operate_as(office_seed(h), AdminCommand::ScheduleCorporateAction(own));
    rejected(&events, "only a passed vote");

    // The proposals apply an hour and a half before a rollover at least seven days on.
    let rollover = h.engine.params().expect("parameters are set").rollover.clone();
    let mut due = rollover.next_after(h.ts + NOTICE_MS);
    if due - 90 * MINUTE_MS < h.ts + NOTICE_MS {
        due = rollover.next_after(due);
    }
    let applies_at = due - 90 * MINUTE_MS;
    let earlier = Rollover { hour: (rollover.hour + 23) % 24, ..rollover };
    let tighter = CorporateBounds { max_dividend_bps: 800, ..LAUNCH_CORPORATE_BOUNDS };
    let moved = AdminCommand::SetParams(ProtocolParams {
        rollover: earlier,
        corporate_bounds: tighter,
        ..h.engine.baseline_params().expect("parameters are set")
    });
    let (close, open) = session_closes(&aapl, applies_at, 2);
    let mark = h.engine.markets()[&AAPL].mark.expect("AAPL has traded");
    let above_cap = AdminCommand::ScheduleCorporateAction(dividend("voted", mark.mul_bps(1_500), close, open));
    let split = AdminCommand::ScheduleCorporateAction(CorporateAction {
        id: "wide".into(),
        market: AAPL,
        previous_close: close,
        ex_open: open,
        adjustment: Adjustment::Split { new_units: 21, old_units: 1 },
    });
    let past_launch = PriceRules { max_age_ms: launch.max_age_ms + 1, ..launch };
    let spec = h.engine.baseline_spec(BTC).expect("BTC is listed");
    let relisting = AdminCommand::UpsertMarket(MarketSpec { price_rules: past_launch, ..spec });
    let proposals = [(191, moved), (192, above_cap), (193, split), (194, relisting)];
    for (proposal, command) in &proposals {
        let events = pass(h, *proposal, false, Some(command.clone()));
        assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    }
    h.advance_to(applies_at);
    let [(_, moved), (_, above_cap), (_, split), (_, relisting)] = proposals;
    applied(&apply(h, 191, moved));
    assert_eq!(h.engine.next_rollover(), Some(due), "an earlier rollover time leaves the due rollover due");
    assert_eq!(h.engine.params().expect("parameters are set").corporate_bounds, tighter);
    let events = apply(h, 192, above_cap);
    applied(&events);
    assert!(events.iter().any(|event| matches!(event, Event::CorporateActionScheduled { .. })), "{events:?}");
    rejected(&apply(h, 193, split), "a split's ratio is at most 20 either way");
    rejected(&apply(h, 194, relisting), "looser than its launch rules");
    assert_eq!(h.engine.markets()[&BTC].spec.price_rules, launch);

    // The due rollover charges a day; the next, 23 hours on, 23 hours of one.
    h.advance_to(due);
    let one_day = financing(&h.cmd(Command::Tick), &client);
    assert!(one_day.is_positive(), "the long pays financing");
    let next = h.engine.next_rollover().expect("rollovers are scheduled");
    assert_eq!(next, due + 23 * HOUR_MS);
    h.advance_to(next);
    let short_day = financing(&h.cmd(Command::Tick), &client);
    let hours = |n| Fixed::from_int(n * HOUR_MS);
    assert_eq!(short_day, one_day.mul_div(hours(23), hours(24), Rounding::Up));
}

/// Sequences the node's `ApplyProposal` for `proposal` with `command`.
fn apply(h: &mut Harness, proposal: u64, command: AdminCommand) -> Vec<Event> {
    h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command })))
}

fn applied(events: &[Event]) {
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}

fn rejected(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { reason: r }) if r.contains(reason)), "{events:?}");
}

/// A dividend of `gross` a share on AAPL, nothing withheld, before `close` and ex at `open`.
fn dividend(id: &str, gross: Fixed, close: Millis, open: Millis) -> CorporateAction {
    let adjustment = Adjustment::Dividend { gross_per_unit: gross, withholding_rate: f("0") };
    CorporateAction { id: id.into(), market: AAPL, previous_close: close, ex_open: open, adjustment }
}

/// The `count`th session close of `schedule` after `after`, and the open after it, found minute by
/// minute.
fn session_closes(schedule: &Schedule, after: Millis, count: usize) -> (Millis, Millis) {
    let minutes = |from: Millis| (from / MINUTE_MS + 1..).map(|minute| minute * MINUTE_MS);
    let close = minutes(after)
        .filter(|at| schedule.is_open(at - 1) && !schedule.is_open(*at))
        .nth(count - 1)
        .expect("the schedule closes");
    let open = minutes(close).find(|at| schedule.is_open(*at)).expect("the schedule opens");
    (close, open)
}

/// The seed of the key in office, among the session's.
fn office_seed(h: &Harness) -> u8 {
    let office = h.engine.operator().expect("the operator is recorded").address;
    (1..=u8::MAX).find(|seed| Client::new(*seed).account == office).expect("a session key is in office")
}

/// What `client` paid in financing among `events`.
fn financing(events: &[Event], client: &Client) -> Fixed {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Financing { account, amount, .. } if *account == client.account => Some(*amount),
            _ => None,
        })
        .sum()
}
