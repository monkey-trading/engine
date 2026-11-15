//! The session's `V61` part: volume tier fixes. A vote activates the version, and a wallet's matured
//! close and a later close of pending units at the same index and time stay two closing lots, so a
//! reopening restores only the units the toxicity score matches.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Fill};
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;
use engine_core::volume_tier::{MIN_HOLD_MS, launch_volume_tiers};

use crate::common::close_spread::{attest, fund, trade};
use crate::common::{BTC, Client, Harness, f};
use crate::v56::pass;

/// The seed of the wallet this part trades through.
const CLIENT_SEED: u8 = 0x68;

/// `V61` activates by vote and leaves the table as `V59` set it. The wallet opens a BTC long, holds it
/// past `MIN_HOLD_MS`, and within one sequenced millisecond sells 0.5 at one index, buys 0.1 at
/// another, sells 0.1 at the first and buys 0.1 at it: the first close keeps its 0.5 matured units.
pub(crate) fn session(h: &mut Harness) {
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V61 };
    let events = pass(h, 30, false, Some(activate.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 30, command: activate })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V61);
    assert_eq!(h.engine.params().expect("set").volume_tiers, launch_volume_tiers());
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "2000000");
    h.price(BTC, "100000");
    only_fill(&trade(h, &client, BTC, Side::Buy, "1", "100000"));
    h.advance(MIN_HOLD_MS);
    let closes = [
        (Side::Sell, "0.5", "100000"),
        (Side::Buy, "0.1", "100100"),
        (Side::Sell, "0.1", "100000"),
        (Side::Buy, "0.1", "100000"),
    ];
    for (side, qty, mid) in closes {
        h.price(BTC, mid);
        only_fill(&trade(h, &client, BTC, side, qty, mid));
    }
    let book = h.engine.account_volume(client.account).expect("counted");
    assert_eq!((book.closing.len(), book.closing[0].qty), (1, f("0.5")), "{book:?}");
}

/// The one fill in `events`.
fn only_fill(events: &[Event]) -> Fill {
    crate::common::events::only_fill(events).clone()
}
