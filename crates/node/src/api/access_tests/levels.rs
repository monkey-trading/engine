//! Take-profit and stop-loss levels, from `V79`, through the API: an order names them in its signed body,
//! `POST /v1/positions/levels` sets, moves and removes them on one screened request, the account read
//! shows them beside each position, and the market read names each market's minimum distance.

use engine_core::command::PositionLevelsRequest;
use engine_core::levels::PositionLevels;
use engine_core::types::MarketId;

use super::session_keys::registration;
use super::*;
use crate::clock::Clock;

const BTC: MarketId = 1;

/// A take profit and a stop loss, each optional, written as decimals.
pub(super) fn levels(take_profit: Option<&str>, stop_loss: Option<&str>) -> PositionLevels {
    let parse = |level: Option<&str>| level.map(|level| level.parse().unwrap());
    PositionLevels { take_profit: parse(take_profit), stop_loss: parse(stop_loss) }
}

/// A market order quoted at `quoted`.
pub(super) fn market(quoted: &str) -> OrderKind {
    OrderKind::Market { quoted: quoted.parse().unwrap(), max_slippage_bps: None }
}

/// `client`'s order of `qty` in `market`, naming `levels`, signed with its next nonce and an expiry a
/// minute from the node's time, as a JSON body.
async fn signed_order(
    node: &Node,
    client: &SigningKey,
    market: MarketId,
    side: Side,
    qty: &str,
    kind: OrderKind,
    levels: PositionLevels,
) -> Value {
    let account = eip712::address(client.verifying_key());
    let nonce = node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap();
    let expires_at = node.clock.now_ms() + 60_000;
    let mut request = OrderRequest {
        account,
        client_id: nonce,
        market,
        side,
        qty: qty.parse().unwrap(),
        kind,
        reduce_only: false,
        take_profit: levels.take_profit,
        stop_loss: levels.stop_loss,
        auth: None,
    };
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(client, &hash) });
    json!(request)
}

/// `signed_order` through the order route: its status and events.
pub(super) async fn order(
    node: &Node,
    client: &SigningKey,
    market: MarketId,
    side: Side,
    qty: &str,
    kind: OrderKind,
    levels: PositionLevels,
) -> (StatusCode, Value) {
    let body = signed_order(node, client, market, side, qty, kind, levels).await;
    node.post("/v1/orders", body).await
}

/// `client`'s request setting its levels in `market` to `levels`, signed by `signer` with `nonce`,
/// expiring at `expires_at`, as a JSON body.
pub(super) fn levels_body(
    client: &SigningKey,
    signer: &SigningKey,
    nonce: u64,
    expires_at: i64,
    market: MarketId,
    levels: PositionLevels,
) -> Value {
    let account = eip712::address(client.verifying_key());
    let PositionLevels { take_profit, stop_loss } = levels;
    let mut request = PositionLevelsRequest { account, market, take_profit, stop_loss, auth: None };
    let hash = eip712::position_levels_hash(&eip712::domain(31_337, VAULT), &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(signer, &hash) });
    json!(request)
}

/// `levels_body` signed by `signer` with its next nonce and an expiry a minute from the node's time,
/// through the levels route: its status and events.
pub(super) async fn set_levels(
    node: &Node,
    client: &SigningKey,
    signer: &SigningKey,
    market: MarketId,
    levels: PositionLevels,
) -> (StatusCode, Value) {
    let signer_id = eip712::address(signer.verifying_key());
    let nonce = node.state.query(move |engine| engine.next_request_nonce(signer_id)).await.unwrap();
    let body = levels_body(client, signer, nonce, node.clock.now_ms() + 60_000, market, levels);
    node.post("/v1/positions/levels", body).await
}

/// The events of `events` of type `kind`.
pub(super) fn of_type<'a>(events: &'a Value, kind: &str) -> Vec<&'a Value> {
    events.as_array().unwrap().iter().filter(|event| event["type"] == kind).collect()
}

/// The commands in `node`'s log `matches` selects.
fn logged(node: &Node, matches: impl Fn(&Command) -> bool) -> usize {
    node.entries().iter().filter(|entry| matches!(entry, LogEntry::Command(c) if matches(&c.sequenced.cmd))).count()
}

fn is_levels_request(cmd: &Command) -> bool {
    matches!(cmd, Command::AccessSetPositionLevels { .. })
}

#[tokio::test]
async fn an_order_naming_levels_is_signed_over_them_and_the_account_read_shows_them_beside_its_position() {
    let node = Node::start("levels-order", true).await;
    let client = key(0x71);
    let id = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let named = levels(Some("102000"), Some("98000"));

    // A level altered after signing fails the signature, so the order never reaches the log.
    let before = node.entries().len();
    let mut altered = signed_order(&node, &client, BTC, Side::Buy, "1", market("100000"), named).await;
    altered["stop_loss"] = json!("99000");
    assert_eq!(node.post("/v1/orders", altered).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(node.entries().len(), before);

    let (status, events) = order(&node, &client, BTC, Side::Buy, "1", market("100000"), named).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(of_type(&events, "fill").len(), 1, "{events}");
    let set = json!({ "type": "position_levels_set", "account": id, "market": BTC, "take_profit": "102000",
        "stop_loss": "98000" });
    assert_eq!(of_type(&events, "position_levels_set"), [&set], "{events}");
    let read = node.get(&format!("/v1/accounts/{id}")).await;
    assert_eq!(read["positions"]["1"]["qty"], "1", "{read}");
    assert_eq!(read["levels"], json!({ "1": { "take_profit": "102000", "stop_loss": "98000" } }), "{read}");

    // A level the request leaves out is removed and left out of the read; with neither, so is the market.
    let (status, events) = set_levels(&node, &client, &client, BTC, levels(None, Some("98000"))).await;
    assert_eq!((status, of_type(&events, "position_levels_set").len()), (StatusCode::OK, 1), "{events}");
    let read = node.get(&format!("/v1/accounts/{id}")).await;
    assert_eq!(read["levels"], json!({ "1": { "stop_loss": "98000" } }), "{read}");
    let (status, events) = set_levels(&node, &client, &client, BTC, levels(None, None)).await;
    assert_eq!((status, of_type(&events, "position_levels_set").len()), (StatusCode::OK, 1), "{events}");
    let read = node.get(&format!("/v1/accounts/{id}")).await;
    assert!(read.get("levels").is_none(), "{read}");
}

#[tokio::test]
async fn a_levels_request_is_one_screened_log_entry_on_one_nonce_and_a_session_key_may_send_it() {
    let node = Node::start("levels-request", true).await;
    let (client, session) = (key(0x72), key(0x73));
    let (id, session_id) = (eip712::address(client.verifying_key()), eip712::address(session.verifying_key()));
    node.onboard(&client).await;
    let (status, events) = order(&node, &client, BTC, Side::Buy, "1", market("100000"), levels(None, None)).await;
    assert_eq!((status, of_type(&events, "fill").len()), (StatusCode::OK, 1), "{events}");

    // One request sets both levels: one screening, one log entry, one nonce. With no fresh decision kept,
    // the screening is one call, which asks about the account and its address and names no request.
    node.forget_decisions();
    let screened = node.source.lock().unwrap().requests.len();
    let body = levels_body(&client, &client, 2, NOW + 60_000, BTC, levels(Some("102000"), Some("98000")));
    let (status, events) = node.post("/v1/positions/levels", body.clone()).await;
    let set = json!({ "type": "position_levels_set", "account": id, "market": BTC, "take_profit": "102000",
        "stop_loss": "98000" });
    assert_eq!((status, events), (StatusCode::OK, json!([set])));
    let requests = node.source.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), screened + 1, "one screening request");
    assert_eq!(requests[screened], json!({ "account": id, "payer": null, "peer": "127.0.0.1" }));
    assert_eq!(logged(&node, is_levels_request), 1);
    assert_eq!(node.get(&format!("/v1/accounts/{id}/nonce")).await["next_nonce"], 3);

    // A stop loss inside BTC's 30 bps of the long's closing price, 99,980, is refused naming both.
    let (status, events) = set_levels(&node, &client, &client, BTC, levels(Some("102000"), Some("99800"))).await;
    let reason = json!({ "kind": "level_too_close", "level": "99800", "min_distance_bps": 30, "reference": "99980" });
    let refused = json!({ "type": "order_rejected", "account": id, "client_id": 0, "reason": reason });
    assert_eq!((status, events), (StatusCode::OK, json!([refused])));

    // A registered session key moves both levels on its own nonce sequence.
    let (status, events) = node.post("/v1/session-keys", registration(&client, &session, 4, NOW + 3_600_000)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let (status, events) = set_levels(&node, &client, &session, BTC, levels(Some("103000"), Some("97000"))).await;
    assert_eq!((status, of_type(&events, "position_levels_set").len()), (StatusCode::OK, 1), "{events}");
    assert_eq!(node.get(&format!("/v1/accounts/{session_id}/nonce")).await["next_nonce"], 1);
    let read = node.get(&format!("/v1/accounts/{id}")).await;
    assert_eq!(read["levels"], json!({ "1": { "take_profit": "103000", "stop_loss": "97000" } }), "{read}");
    assert_eq!(logged(&node, is_levels_request), 3);
}

#[tokio::test]
async fn levels_requests_are_refused_for_a_stale_nonce_a_forged_or_altered_signature_and_a_stranger() {
    let node = Node::start("levels-refusals", true).await;
    let (client, stranger) = (key(0x74), key(0x75));
    node.onboard(&client).await;
    let named = levels(None, Some("98000"));
    let (entries, screened) = (node.entries().len(), node.source.lock().unwrap().requests.len());

    let (status, body) =
        node.post("/v1/positions/levels", levels_body(&client, &client, 7, NOW + 60_000, BTC, named)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::CONFLICT, Some("invalid nonce")));
    let forged = levels_body(&client, &stranger, 1, NOW + 60_000, BTC, named);
    let (status, body) = node.post("/v1/positions/levels", forged).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    let mut altered = levels_body(&client, &client, 1, NOW + 60_000, BTC, named);
    altered["stop_loss"] = json!("1");
    assert_eq!(node.post("/v1/positions/levels", altered).await.0, StatusCode::UNAUTHORIZED);
    let (status, body) =
        node.post("/v1/positions/levels", levels_body(&stranger, &stranger, 0, NOW + 60_000, BTC, named)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some("account has never been credited")));

    assert_eq!(node.entries().len(), entries, "none of them reached the log");
    assert_eq!(node.source.lock().unwrap().requests.len(), screened, "none of them was screened");
}

#[tokio::test]
async fn a_levels_request_refused_by_screening_sets_nothing_and_uses_its_nonce() {
    let node = Node::start("levels-screening", true).await;
    let client = key(0x76);
    let id = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let (status, events) = order(&node, &client, BTC, Side::Buy, "1", market("100000"), levels(None, None)).await;
    assert_eq!((status, of_type(&events, "fill").len()), (StatusCode::OK, 1), "{events}");

    node.source.lock().unwrap().sanctioned.insert(id);
    node.forget_decisions();
    let (status, events) = set_levels(&node, &client, &client, BTC, levels(None, Some("98000"))).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(
        (events.as_array().unwrap().len(), &events[0]["type"], &events[0]["reason"]["reason"]),
        (1, &json!("order_rejected"), &json!("account_sanctioned")),
        "{events}"
    );
    assert!(node.get(&format!("/v1/accounts/{id}")).await.get("levels").is_none());
    assert_eq!(logged(&node, is_levels_request), 1);
    assert_eq!(node.get(&format!("/v1/accounts/{id}/nonce")).await["next_nonce"], 3);
}

#[tokio::test]
async fn the_market_read_names_each_markets_minimum_level_distance() {
    let node = Node::start("levels-markets", true).await;
    let markets = node.get("/v1/markets").await;
    let distances: std::collections::BTreeMap<u64, u64> = markets
        .as_array()
        .unwrap()
        .iter()
        .map(|market| (market["id"].as_u64().unwrap(), market["min_level_distance_bps"].as_u64().unwrap()))
        .collect();
    let configured: std::collections::BTreeMap<u64, u64> = dev_config()
        .genesis
        .markets
        .iter()
        .map(|spec| (u64::from(spec.id), u64::try_from(spec.min_level_distance_bps.unwrap()).unwrap()))
        .collect();
    assert_eq!(distances, configured);
    assert_eq!((distances[&1], distances[&32]), (30, 25), "{distances:?}");
}
