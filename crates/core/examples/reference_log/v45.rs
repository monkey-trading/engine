//! The session's `V45` part: a close-all closes a position past the quantity input limit in
//! pieces of at most `Fixed::MAX_INPUT` units per entry.

use engine_core::Fixed;
use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::batch::{close_all, closes};
use crate::common::close_spread::{attest, fund, trade};
use crate::common::{Client, ETH, Harness, f};

/// The seed of the account key this part trades through.
const ACCOUNT_SEED: u8 = 0x4d;
/// ETH at five hundredths of a cent: 1.2 billion units are a $600,000 position.
const PRICE: &str = "0.0005";

/// An account buys 600 million units twice, so its position (1.2 billion units) is past the input
/// limit, and a close-all closes a billion of it and then the 200 million left, each fill stating
/// what remains in its `position`.
pub(crate) fn session(h: &mut Harness) {
    let account = Client { expires_at: h.ts + 60_000, ..Client::new(ACCOUNT_SEED) };
    attest(h, &account);
    fund(h, &account, "1000000");
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V45 });
    h.price(ETH, PRICE);
    for _ in 0..2 {
        let events = trade(h, &account, ETH, Side::Buy, "600000000", PRICE);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    }
    assert_eq!(h.engine.accounts()[&account.account].position(ETH).qty, f("1200000000"));

    for (closed, left) in [(Fixed::MAX_INPUT, f("200000000")), (f("200000000"), Fixed::ZERO)] {
        let nonce = h.engine.next_request_nonce(account.account);
        let events = h.cmd(close_all(h, &account, account.account, nonce, closes(&[ETH], PRICE), None));
        assert!(
            matches!(&events[..], [Event::OrderAccepted { .. }, Event::Fill(fill)] if fill.qty == closed && fill.position == left),
            "{events:?}"
        );
    }
}
