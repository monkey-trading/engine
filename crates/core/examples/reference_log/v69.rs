//! The session's `V69` part: a rollover reads an account's tier from the volume that had counted by its
//! own time. A vote activates the version. A client's AAPL long, which a lower price leaves short of the
//! weekend margin, is closed out when the gap starts, in a catch-up that a command stamped two and a half
//! hours later makes and that also runs the rollover: the client's BTC lot, bought half a minute before
//! the gap, matures after the close-out's boundary and the close-out's own count falls after it, so the
//! rollover's tier leaves both out. The session's launch table gives tier 1 no financing discount, so the
//! tier-2 case the rule changes is pinned by `tests/tier_catch_up.rs`.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};

use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::{fills, only_fill};
use crate::common::{AAPL, BTC, Client, Harness};
use crate::v56::pass;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x7d;
const HOUR_MS: Millis = 60 * 60 * 1000;

/// Activates `V69` by vote; then the close-out and catch-up described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V69 };
    let events = pass(h, 120, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 120, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V69);
    // An activation leaves the boundaries its notice spanned to the next command.
    h.cmd(Command::Tick);

    // The next weekend gap: a client opens an AAPL long three hours before it.
    let close = h.engine.markets()[&AAPL].weekend.expect("AAPL keeps its next gap").close;
    h.advance_to(close - 3 * HOUR_MS);
    h.cmd(Command::Tick);
    let client = Client::new(CLIENT_SEED);
    attest(h, &client);
    fund(h, &client, "20000");
    let btc = h.engine.markets()[&BTC].mark.expect("BTC has traded").to_string();
    h.price(AAPL, "250");
    only_fill(&trade(h, &client, AAPL, Side::Buy, "190", "250"));
    // Half a minute before the gap it buys a little BTC, which counts from half a minute after it.
    h.advance_to(close - 30_000);
    h.price(BTC, &btc);
    h.price(AAPL, "250");
    only_fill(&trade(h, &client, BTC, Side::Buy, "0.01", &btc));
    // A price a fifth below leaves the long short of the weekend margin when the gap starts.
    h.advance_to(close - 1_000);
    h.price(AAPL, "200");

    // A command two and a half hours after the gap starts catches up the close-out and the rollover at it.
    h.advance_to(close + 150 * 60_000);
    let events = h.cmd(Command::Tick);
    assert!(
        fills(&events).iter().any(|fill| fill.liquidation && fill.account == client.account && fill.market == AAPL),
        "{events:?}"
    );
    assert!(events.iter().any(|event| matches!(event, Event::Financing { account, .. } if *account == client.account)));
}
