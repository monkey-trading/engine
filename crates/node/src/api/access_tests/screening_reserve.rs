//! The screening reserve goes to requests the API's engine copy shows can reduce risk: a reduce-only
//! order or a position's take profit and stop loss in a market the account holds a position in, a
//! close-all naming one, and a withdrawal while the account has withdrawable cash. Every other request
//! takes a slot outside the reserve, as an order does, and an account holds one of the reserve's slots
//! at a time.

use std::future::Future;
use std::pin::Pin;

use axum::http::Method;
use futures_util::future::join_all;

use super::batch::close_all;
use super::levels::{levels, levels_body};
use super::pre_deposit::{address, next_nonce, screened};
use super::pre_deposit_budgets::send;
use super::screening_slots::{fills, hold, production_slots, signed_order, until_in_flight};
use super::*;

/// Four wallets that hold no position each send two reduce-only orders, two wallets from each of two
/// client addresses. The orders take only the six slots outside the reserve, the last two are told to
/// retry, and a trader holding a position still sets its stop loss, closes it and withdraws.
#[tokio::test]
async fn reduce_only_orders_from_wallets_with_no_position_leave_the_reserve_to_closes_and_withdrawals() {
    let mut node = Node::start("screening-reserve-flagged", true).await;
    production_slots(&mut node);
    let wallets = [key(0x91), key(0x92), key(0x93), key(0x95)];
    let trader = key(0x94);
    for wallet in wallets.iter().chain([&trader]) {
        node.onboard(wallet).await;
    }
    let (status, events) = node.post("/v1/orders", order(&trader, 1)).await;
    assert_eq!(status, StatusCode::OK, "the trader opens a position: {events}");

    let gate = hold(&node, &wallets.iter().collect::<Vec<_>>());
    let asked = screened(&node);
    // Each wallet signs two different reduce-only orders on its next nonce.
    let flagged =
        |client, wallet, id| send(&node, client, Method::POST, "/v1/orders", signed_order(wallet, wallet, 1, id, true));
    let mut held = Box::pin(join_all([
        flagged(1, &wallets[0], 1),
        flagged(1, &wallets[0], 2),
        flagged(1, &wallets[1], 1),
        flagged(1, &wallets[1], 2),
        flagged(2, &wallets[2], 1),
        flagged(2, &wallets[2], 2),
        flagged(2, &wallets[3], 1),
        flagged(2, &wallets[3], 2),
    ]));
    assert!(until_in_flight(&node, &mut held, asked + 6).await.is_none());
    assert_eq!(screened(&node) - asked, 6, "orders that reduce nothing take only the slots outside the reserve");

    // Each of the trader's requests finds no fresh decision kept, so calls the source and takes a slot.
    let before = screened(&node);
    let nonce = next_nonce(&node, address(&trader)).await;
    let stop = levels_body(&trader, &trader, nonce, NOW + 60_000, 1, levels(None, Some("99000")));
    let stop = send(&node, 3, Method::POST, "/v1/positions/levels", stop).await;
    node.forget_decisions();
    let nonce = next_nonce(&node, address(&trader)).await;
    let close = send(&node, 3, Method::POST, "/v1/orders", signed_order(&trader, &trader, nonce, 2, true)).await;
    node.forget_decisions();
    let nonce = next_nonce(&node, address(&trader)).await;
    let withdraw = send(&node, 3, Method::POST, "/v1/withdrawals", withdrawal(&trader, nonce)).await;
    assert_eq!(screened(&node) - before, 3, "the stop loss, close and withdrawal each took a slot");
    gate.add_permits(8);
    let statuses: Vec<_> = held.await.into_iter().map(|(status, ..)| status).collect();
    let retried = statuses.iter().filter(|status| **status == StatusCode::SERVICE_UNAVAILABLE).count();
    assert_eq!(retried, 2, "the orders past the slots outside the reserve: {statuses:?}");
    assert_eq!(
        [stop.0, close.0, withdraw.0],
        [StatusCode::OK; 3],
        "the trader's stop loss, close and withdrawal: {stop:?} {close:?} {withdraw:?}"
    );
}

/// While every slot outside the reserve is held, each kind of request that claims to reduce risk is
/// told to retry from a wallet with no position and no withdrawable cash, and takes the reserve from a
/// trader with both: a stop loss, a reduce-only order, a withdrawal, and a close-all naming one market
/// the trader holds and one it does not.
#[tokio::test]
async fn a_request_takes_the_reserve_only_where_its_account_has_something_to_reduce() {
    let mut node = Node::start("screening-reserve-cases", true).await;
    production_slots(&mut node);
    let fillers = [key(0x91), key(0x92), key(0x93)];
    let (trader, idle) = (key(0x94), key(0x96));
    for wallet in fillers.iter().chain([&trader, &idle]) {
        node.onboard(wallet).await;
    }
    for nonce in 1..=2 {
        let (status, events) = node.post("/v1/orders", order(&trader, nonce)).await;
        assert_eq!(status, StatusCode::OK, "the trader buys 0.01 BTC: {events}");
    }
    let (status, events) = node.post("/v1/withdrawals", withdrawal_of(&idle, 1, Fixed::from_int(100_000))).await;
    assert_eq!((status, &events[0]["type"]), (StatusCode::OK, &json!("withdrawal_queued")), "{events}");
    let idle_account = address(&idle);
    let cash = node.state.query(move |engine| engine.withdrawable(idle_account)).await.unwrap();
    assert_eq!(cash, Fixed::ZERO, "the idle wallet has withdrawn all its cash");
    let (gate, held) = fill_outside_the_reserve(&node, &fillers, &[]).await;

    let cases: [Case; 4] = [
        (
            "/v1/positions/levels",
            |wallet, nonce| levels_body(wallet, wallet, nonce, NOW + 60_000, 1, levels(None, Some("99000"))),
            "position_levels_set",
        ),
        ("/v1/orders", |wallet, nonce| signed_order(wallet, wallet, nonce, 2, true), "fill"),
        ("/v1/withdrawals", withdrawal, "withdrawal_queued"),
        ("/v1/positions/close-all", |wallet, nonce| close_all(wallet, wallet, nonce, &[2, 1], 100_000), "fill"),
    ];
    for (path, body, effect) in cases {
        let nonce = next_nonce(&node, address(&idle)).await;
        let (status, refusal, _) = send(&node, 3, Method::POST, path, body(&idle, nonce)).await;
        assert_eq!(
            (status, refusal["error"].as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, Some("screening is at capacity")),
            "{path} from the wallet with nothing to reduce"
        );
        // With no fresh decision kept, the trader's request calls the source and takes a slot.
        node.forget_decisions();
        let (nonce, before) = (next_nonce(&node, address(&trader)).await, screened(&node));
        let (status, events, _) = send(&node, 4, Method::POST, path, body(&trader, nonce)).await;
        let done = events.as_array().is_some_and(|events| events.iter().any(|event| event["type"] == effect));
        assert!(status == StatusCode::OK && done, "{path} from the trader takes the reserve: {status} {events}");
        assert_eq!(screened(&node), before + 1, "{path} from the trader called the source");
    }
    gate.add_permits(6);
    assert_eq!(held.await.into_iter().map(|(status, ..)| status).collect::<Vec<_>>(), [StatusCode::OK; 6]);
}

/// While every slot outside the reserve is held, a trader's stop loss being screened holds one of the
/// reserved slots: its withdrawal is told to retry, and another account's withdrawal takes the other.
#[tokio::test]
async fn an_account_holds_one_reserved_slot_at_a_time() {
    let mut node = Node::start("screening-reserve-one", true).await;
    production_slots(&mut node);
    let fillers = [key(0x91), key(0x92), key(0x93)];
    let (trader, other) = (key(0x94), key(0x96));
    for wallet in fillers.iter().chain([&trader, &other]) {
        node.onboard(wallet).await;
    }
    let (status, events) = node.post("/v1/orders", order(&trader, 1)).await;
    assert!(status == StatusCode::OK && fills(&events), "the trader opens a position: {events}");
    let held_too = [&trader];
    let (gate, held) = fill_outside_the_reserve(&node, &fillers, &held_too).await;

    let asked = screened(&node);
    let stop = levels_body(&trader, &trader, 2, NOW + 60_000, 1, levels(None, Some("99000")));
    let mut stop = Box::pin(send(&node, 3, Method::POST, "/v1/positions/levels", stop));
    assert!(until_in_flight(&node, &mut stop, asked + 1).await.is_none(), "the stop loss takes a reserved slot");
    let (status, refusal, retry_after) = send(&node, 4, Method::POST, "/v1/withdrawals", withdrawal(&trader, 2)).await;
    assert_eq!(
        (status, refusal["error"].as_str(), retry_after),
        (StatusCode::SERVICE_UNAVAILABLE, Some("screening is at capacity"), Some(5)),
        "the trader's second request"
    );
    let (status, events, _) = send(&node, 5, Method::POST, "/v1/withdrawals", withdrawal(&other, 1)).await;
    assert_eq!((status, &events[0]["type"]), (StatusCode::OK, &json!("withdrawal_queued")), "{events}");

    gate.add_permits(7);
    let (status, events, _) = stop.await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert_eq!(held.await.into_iter().map(|(status, ..)| status).collect::<Vec<_>>(), [StatusCode::OK; 6]);
}

// ── Helpers ──

/// The status, JSON body and `Retry-After` of each held request.
type Answers = Vec<(StatusCode, Value, Option<u64>)>;

/// A route, the body a wallet signs for it at a nonce, and the event the body produces when it runs.
type Case = (&'static str, fn(&SigningKey, u64) -> Value, &'static str);

/// Holds every slot outside the reserve with six new orders the source holds: two from each of
/// `fillers`, the first two from one client and the third from another. Returns the gate that releases
/// them, and the screenings of `also` the source holds too, and the orders' answers.
async fn fill_outside_the_reserve(
    node: &Node,
    fillers: &[SigningKey; 3],
    also: &[&SigningKey],
) -> (Arc<Semaphore>, Pin<Box<impl Future<Output = Answers>>>) {
    let gate = hold(node, &fillers.iter().chain(also.iter().copied()).collect::<Vec<_>>());
    let asked = screened(node);
    let opening =
        |client, wallet, id| send(node, client, Method::POST, "/v1/orders", signed_order(wallet, wallet, 1, id, false));
    let mut held = Box::pin(join_all([
        opening(1, &fillers[0], 1),
        opening(1, &fillers[0], 2),
        opening(1, &fillers[1], 1),
        opening(1, &fillers[1], 2),
        opening(2, &fillers[2], 1),
        opening(2, &fillers[2], 2),
    ]));
    assert!(until_in_flight(node, &mut held, asked + 6).await.is_none());
    assert_eq!(screened(node) - asked, 6, "the slots outside the reserve are held");
    (gate, held)
}

/// A signed withdrawal of `amount` by `key`'s account at `nonce`.
fn withdrawal_of(key: &SigningKey, nonce: u64, amount: Fixed) -> Value {
    let account = address(key);
    let hash = eip712::withdrawal_hash(&eip712::domain(31_337, VAULT), account, amount, nonce, NOW + 60_000);
    let auth = RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(key, &hash) };
    json!(WithdrawalRequest { account, amount, auth: Some(auth) })
}
