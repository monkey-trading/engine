//! Take-profit and stop-loss harness: the `V79` setup, orders naming levels, the signed request that
//! sets them, and readers of their events.

use engine_core::Fixed;
use engine_core::auth::RequestAuth;
use engine_core::command::{Command, OrderKind, OrderRequest, PositionLevelsRequest};
use engine_core::eip712;
use engine_core::event::{Event, Reject};
use engine_core::levels::{LevelKind, LevelsRemoved, PositionLevels};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Side};

use crate::common::close_spread::evidence;
use crate::common::session_keys::funded_at;
use crate::common::{Client, Harness, f};

/// The `V79` access harness: client 1 declared and holding 100000 USD, client 7 not yet onboarded, and
/// BTC priced at 100000. BTC's minimum level distance is its 20bp ceiling.
pub fn funded() -> (Harness, Client, Client) {
    funded_at(ProtocolVersion::V79)
}

/// A take profit and a stop loss, each optional, written as decimals.
pub fn levels(take_profit: Option<&str>, stop_loss: Option<&str>) -> PositionLevels {
    PositionLevels { take_profit: take_profit.map(f), stop_loss: stop_loss.map(f) }
}

/// The client's signed, screened order of `kind` naming `levels`.
#[allow(clippy::too_many_arguments)]
pub fn order_with(
    h: &mut Harness,
    client: &Client,
    market: MarketId,
    side: Side,
    qty: &str,
    kind: OrderKind,
    reduce_only: bool,
    levels: PositionLevels,
) -> Vec<Event> {
    let mut request = OrderRequest {
        account: client.account,
        client_id: 7,
        market,
        side,
        qty: f(qty),
        kind,
        reduce_only,
        take_profit: levels.take_profit,
        stop_loss: levels.stop_loss,
        auth: None,
    };
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = h.ts + 60_000;
    let hash = eip712::order_hash(&client.domain, &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) });
    let ev = evidence(h, hash, Some(client.account), None);
    h.cmd(Command::AccessOrder { request, evidence: ev })
}

/// The client's market order for `qty` quoted at `quoted`, naming `levels`.
pub fn open_with(
    h: &mut Harness,
    client: &Client,
    market: MarketId,
    side: Side,
    qty: &str,
    quoted: &str,
    levels: PositionLevels,
) -> Vec<Event> {
    let kind = OrderKind::Market { quoted: f(quoted), max_slippage_bps: None };
    order_with(h, client, market, side, qty, kind, false, levels)
}

/// The signed, screened request setting the client's levels in `market` to `levels`.
pub fn set(h: &mut Harness, client: &Client, market: MarketId, levels: PositionLevels) -> Vec<Event> {
    let cmd = set_cmd(h, client, market, levels);
    h.cmd(cmd)
}

/// `set` as a command, for sequencing elsewhere.
pub fn set_cmd(h: &Harness, client: &Client, market: MarketId, levels: PositionLevels) -> Command {
    let mut request = PositionLevelsRequest {
        account: client.account,
        market,
        take_profit: levels.take_profit,
        stop_loss: levels.stop_loss,
        auth: None,
    };
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = h.ts + 60_000;
    let hash = eip712::position_levels_hash(&client.domain, &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) });
    let ev = evidence(h, hash, Some(client.account), None);
    Command::AccessSetPositionLevels { request, evidence: ev }
}

/// The levels `client`'s position in `market` holds.
pub fn held(h: &Harness, client: &Client, market: MarketId) -> Option<PositionLevels> {
    h.engine.accounts()[&client.account].levels.get(&market).copied()
}

/// The levels `events` set, for the one `PositionLevelsSet` they hold.
pub fn set_event(events: &[Event]) -> PositionLevels {
    let mut set = events.iter().filter_map(|event| match event {
        Event::PositionLevelsSet { take_profit, stop_loss, .. } => {
            Some(PositionLevels { take_profit: *take_profit, stop_loss: *stop_loss })
        }
        _ => None,
    });
    let levels = set.next().unwrap_or_else(|| panic!("no levels set: {events:?}"));
    assert!(set.next().is_none(), "levels set twice: {events:?}");
    levels
}

/// Why `events` removed levels, for the one `PositionLevelsRemoved` they hold.
pub fn removed(events: &[Event]) -> LevelsRemoved {
    let mut removed = events.iter().filter_map(|event| match event {
        Event::PositionLevelsRemoved { reason, .. } => Some(*reason),
        _ => None,
    });
    let reason = removed.next().unwrap_or_else(|| panic!("no levels removed: {events:?}"));
    assert!(removed.next().is_none(), "levels removed twice: {events:?}");
    reason
}

/// The level `events` triggered, with the order id its fill carries.
pub fn triggered(events: &[Event]) -> Option<(LevelKind, u64)> {
    events.iter().find_map(|event| match event {
        Event::PositionLevelTriggered { level, order_id, .. } => Some((*level, *order_id)),
        _ => None,
    })
}

/// The rejection of the request or order `events` refused.
pub fn refused(events: &[Event]) -> &Reject {
    events
        .iter()
        .find_map(|event| match event {
            Event::OrderRejected { reason, .. } => Some(reason),
            _ => None,
        })
        .unwrap_or_else(|| panic!("not refused: {events:?}"))
}

/// What closing `client`'s whole position in `market` pays per unit now, from the account's quote.
pub fn closing_price(h: &Harness, client: &Client, market: MarketId) -> Fixed {
    let qty = h.engine.accounts()[&client.account].positions[&market].qty;
    let side = if qty.is_positive() { Side::Sell } else { Side::Buy };
    h.engine.quote(client.account, market, side, qty.abs()).expect("the position prices").price
}
