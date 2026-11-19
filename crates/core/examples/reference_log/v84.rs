//! The session's `V84` part: new risk and withdrawals under portfolio margin, while votes govern. A vote
//! activates the version over the grid `V81`'s part left in force. A client opens BTC long against ETH short
//! on a fifth more than BTC's bands, less than per-position margin asks for the pair, withdraws down to its
//! portfolio requirement and is refused a dollar more, is refused closing BTC alone by an order, and closes it
//! with a one-entry close-all. It reopens the pair on fresh cash, withdraws down to its requirement again, and
//! a stop on BTC fires on BTC's fall and fills, though an order closing BTC would be refused.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::fixed::Rounding;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Side};

use crate::common::batch::{close_all, closes};
use crate::common::close_spread::{attest, fund, reduce, rest};
use crate::common::events::fills;
use crate::common::weekend::withdraw;
use crate::common::{BTC, Client, ETH, Harness};
use crate::v81::{applied, mark, open_pair, qty, scheduled};

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x84;

/// Activates `V84` by vote, then the opening, withdrawal, closes and stop above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V84 };
    scheduled(h, 260, &activation);
    h.advance(NOTICE_MS);
    applied(h, 260, activation);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V84);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);
    let (btc, eth) = (mark(h, BTC), mark(h, ETH));
    h.price(BTC, &btc.to_string());
    h.price(ETH, &eth.to_string());

    // The pair on a fifth more than BTC's bands: ETH's short fills where per-position margin refuses it.
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    let (btc_bands, eth_bands) = (bands(h, BTC, btc), bands(h, ETH, eth));
    let cash = btc_bands + btc_bands.mul_bps(2_000);
    fund(h, &client, &cash.to_string());
    open_pair(h, &client, btc, eth);
    let initial = h.engine.margin_standing(client.account).expect("credited").initial_margin;
    assert!(cash < btc_bands + eth_bands && initial < cash, "{initial} on {cash}");

    // A withdrawal down to the portfolio requirement, and not a dollar past it.
    withdraw_to_requirement(h, &client);

    // Closing BTC alone by an order leaves ETH's bands, which the account cannot meet; a close-all can.
    let events = reduce(h, &client, BTC, Side::Sell, &qty(btc).to_string(), &btc.to_string());
    assert!(
        matches!(events.last(), Some(Event::OrderRejected { reason: Reject::InsufficientMargin { .. }, .. })),
        "{events:?}"
    );
    let nonce = h.engine.next_request_nonce(client.account);
    let events = h.cmd(close_all(h, &client, client.account, nonce, closes(&[BTC], &btc.to_string()), Some(10_000)));
    assert_eq!(first_fill(&events), Some((BTC, false)), "{events:?}");
    close_what_is_left(h, &client, eth);

    // The pair again on fresh cash, down to its requirement, with a stop on BTC that an order could not match.
    fund(h, &client, &cash.to_string());
    open_pair(h, &client, btc, eth);
    withdraw_to_requirement(h, &client);
    let stop = btc - btc.mul_bps(100);
    let events = rest(h, &client, BTC, Side::Sell, &qty(btc).to_string(), OrderKind::Stop { price: stop });
    assert!(events.iter().any(|e| matches!(e, Event::OrderAccepted { .. })), "{events:?}");
    let events = h.price(BTC, &(btc - btc.mul_bps(200)).to_string());
    let own = fills(&events).into_iter().find(|fill| fill.account == client.account);
    assert_eq!(own.map(|fill| (fill.market, fill.liquidation)), Some((BTC, false)), "{events:?}");
    h.price(BTC, &btc.to_string());
    close_what_is_left(h, &client, eth);
}

/// `market`'s bands on the $200k a pair leg holds at `price`.
fn bands(h: &Harness, market: MarketId, price: Fixed) -> Fixed {
    h.engine.markets()[&market].spec.margin_for(qty(price).mul(price), Rounding::Up)
}

/// Withdraws `client`'s withdrawable cash in whole dollars, after a dollar more is refused.
fn withdraw_to_requirement(h: &mut Harness, client: &Client) {
    let withdrawable = h.engine.withdrawable(client.account);
    let whole = Fixed::from_int(i64::try_from(withdrawable.raw() / Fixed::ONE.raw()).expect("in range"));
    let events = withdraw(h, client, &(whole + Fixed::ONE).to_string());
    assert!(
        matches!(events.last(), Some(Event::WithdrawalRejected { reason: Reject::InsufficientFunds { .. }, .. })),
        "{events:?}"
    );
    let events = withdraw(h, client, &whole.to_string());
    assert!(events.iter().any(|e| matches!(e, Event::WithdrawalQueued { .. })), "{events:?}");
}

/// Closes `client`'s ETH short, if the close-out after its last command left one: a close that lowers the
/// requirement to nothing.
fn close_what_is_left(h: &mut Harness, client: &Client, eth: Fixed) {
    let held = h.engine.accounts()[&client.account].position(ETH).qty;
    if held.is_negative() {
        let events = reduce(h, client, ETH, Side::Buy, &held.abs().to_string(), &eth.to_string());
        assert_eq!(first_fill(&events), Some((ETH, false)), "{events:?}");
    }
}

/// The market and close-out flag of `events`' first fill.
fn first_fill(events: &[Event]) -> Option<(MarketId, bool)> {
    fills(events).first().map(|fill| (fill.market, fill.liquidation))
}
