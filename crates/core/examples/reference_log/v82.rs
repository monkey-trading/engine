//! The session's `V82` part: a held market's calendar, while votes govern. A vote activates the version. A
//! client goes long AAPL in its next session. The key in office's relisting that shuts AAPL for a session is
//! refused, as votes govern; the same relisting passes as a vote and applies seven days on, though the
//! session it shuts opens within seven days of then, since a vote's seven days are its notice. AAPL stays
//! shut through that session. The operator's own relisting meets the calendar bound only while votes do not
//! govern, which `tests/calendar_notice.rs` covers.

use chrono::DateTime;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule};
use engine_core::types::{Millis, Side};

use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::only_fill;
use crate::common::{AAPL, Client, Harness, f};
use crate::v56::pass;
use crate::v76::office_seed;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x82;
/// The first of this part's proposal ids.
const PROPOSAL: u64 = 240;
const MINUTE_MS: Millis = 60_000;

/// Activates `V82` by vote; then the calendar change described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V82 };
    let events = pass(h, PROPOSAL, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: PROPOSAL, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V82);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    // A client long AAPL in its next session.
    let calendar = h.engine.markets()[&AAPL].spec.schedule.clone();
    let (open, _) = next_session(&calendar, h.ts);
    h.advance_to(open + MINUTE_MS);
    let mark = h.engine.markets()[&AAPL].mark.expect("AAPL has traded").to_string();
    h.price(AAPL, &mark);
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "100000");
    only_fill(&trade(h, &client, AAPL, Side::Buy, "10", &mark));

    // The first session opening after the vote's notice runs, shut by a dated closure.
    let (from, to) = next_session(&calendar, h.ts + NOTICE_MS);
    let mut spec = h.engine.baseline_spec(AAPL).expect("AAPL is listed");
    let Schedule::Weekly { tz, closures, .. } = &mut spec.schedule else { unreachable!("AAPL keeps weekly hours") };
    let local = |at: Millis| DateTime::from_timestamp_millis(at).expect("in range").with_timezone(tz).naive_local();
    closures.push(Period { from: local(from), to: local(to) });
    let relisting = AdminCommand::UpsertMarket(spec);
    let events = h.operate_as(office_seed(h), relisting.clone());
    assert!(
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("only a passed vote")),
        "{events:?}"
    );
    let events = pass(h, PROPOSAL + 1, false, Some(relisting.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    assert!(h.ts < from && from < h.ts + NOTICE_MS, "the closure starts within seven days of the vote's application");
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: PROPOSAL + 1, command: relisting })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");

    // AAPL stays shut through the session the vote closed.
    h.advance_to(from + 60 * MINUTE_MS);
    h.cmd(Command::Tick);
    assert!(!h.engine.markets()[&AAPL].open);
    assert_eq!(h.engine.accounts()[&client.account].position(AAPL).qty, f("10"));
}

/// The first session of `schedule` opening after `after`, as its open and close, found minute by minute.
fn next_session(schedule: &Schedule, after: Millis) -> (Millis, Millis) {
    let minutes = |from: Millis| (from / MINUTE_MS + 1..).map(|minute| minute * MINUTE_MS);
    let open = minutes(after).find(|at| !schedule.is_open(at - 1) && schedule.is_open(*at)).expect("it opens");
    let close = minutes(open).find(|at| !schedule.is_open(*at)).expect("it closes");
    (open, close)
}
