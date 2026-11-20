//! The session's `V86` part: a portfolio close-out step scores a group's later members at the house NAV
//! `Engine::house_nav` reads once the earlier fills are booked. A vote activates the version. A client
//! holds BTC long against ETH short in `V81`'s group, and ETH tripling closes the pair whole, BTC before
//! ETH, ETH scored at the NAV BTC's fill leaves.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::MarketId;

use crate::common::close_spread::{attest, fund};
use crate::common::events::fills;
use crate::common::{BTC, Client, ETH, Harness};
use crate::v81::{applied, mark, open_pair, scheduled};

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x86;
/// The first of this part's proposal ids.
const PROPOSAL: u64 = 280;

/// Activates `V86` by vote, then the close-out above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V86 };
    scheduled(h, PROPOSAL, &activation);
    h.advance(NOTICE_MS);
    applied(h, PROPOSAL, activation);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V86);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);
    let (btc, eth) = (mark(h, BTC), mark(h, ETH));
    h.price(BTC, &btc.to_string());
    h.price(ETH, &eth.to_string());

    // The pair closed out whole: ETH tripling leaves nothing to restore the level but the group.
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "200000");
    open_pair(h, &client, btc, eth);
    // Earlier parts' holders of ETH close out too; the client's close-out is its pair.
    let events = h.price(ETH, &eth.mul_int(3).to_string());
    let own = fills(&events).into_iter().filter(|fill| fill.account == client.account && fill.liquidation);
    let closed: Vec<MarketId> = own.map(|fill| fill.market).collect();
    assert_eq!(closed, vec![BTC, ETH], "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::CloseOut { account, .. } if *account == client.account)));
    h.price(ETH, &eth.to_string());
}
