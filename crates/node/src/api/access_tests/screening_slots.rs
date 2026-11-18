//! The API's screening slots under load: one wallet's requests hold at most its share of them, a
//! client's at most its own, a copy of a signed request or a declared wallet's repeat declaration
//! holds none, new orders leave closes and withdrawals the reserve, and a request refused for want of a
//! slot is told when to retry while the operator hears of it. A position's stop loss takes the reserve
//! as a close does; the refusal carries `Retry-After` and raises `api_screening_slots_full`.

use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;

use axum::http::Method;
use futures_util::future::join_all;

use super::batch::close_all;
use super::levels::{levels, levels_body, of_type};
use super::pre_deposit::{address, next_nonce, screened};
use super::pre_deposit_budgets::send;
use super::session_keys::registration;
use super::*;
use crate::trusted_time::tests::logs::Logs;

/// The screening slots, shares and reserve of `ops/runbooks/production-node.toml`.
const PRODUCTION_SLOTS: &str =
    "max_in_flight=8\nmax_in_flight_per_account=2\nmax_in_flight_per_client=4\nreserved_in_flight=2";

/// A tier-0 wallet and its session key send the eight different requests their two buckets admit at
/// once, from one client. Only the wallet's share reaches the source, and while it is there every
/// other wallet still closes, withdraws and declares.
#[tokio::test(flavor = "multi_thread")]
async fn one_wallets_burst_holds_only_its_share_of_the_screening_slots() {
    let node = production_like("screening-slot-share").await;
    let (wallet, session, victim, newcomer) = (key(0x91), key(0x92), key(0x93), key(0x94));
    node.onboard(&wallet).await;
    node.onboard(&victim).await;
    let (status, events) = node.post("/v1/orders", order(&victim, 1)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let (status, events) = node.post("/v1/session-keys", registration(&wallet, &session, 1, NOW + 3_600_000)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    // Two seconds refill both of the wallet's buckets: four requests each at tier 0's 2 a second.
    tokio::time::sleep(Duration::from_millis(2_200)).await;

    let gate = hold(&node, &[&wallet]);
    let asked = screened(&node);
    let own = (1..=4).map(|id| node.post("/v1/orders", signed_order(&wallet, &wallet, 2, id, false)));
    let keys = (1..=4).map(|id| node.post("/v1/orders", signed_order(&wallet, &session, 0, id, false)));
    let mut burst = Box::pin(join_all(own.chain(keys).collect::<Vec<_>>()));
    assert!(until_in_flight(&node, &mut burst, asked + 1).await.is_none(), "the source holds the wallet's share");
    assert_eq!(screened(&node) - asked, 2, "the wallet's share of the slots");

    let nonce = next_nonce(&node, address(&victim)).await;
    let close = node.post("/v1/positions/close-all", close_all(&victim, &victim, nonce, &[1], 100_000)).await;
    let nonce = next_nonce(&node, address(&victim)).await;
    let withdraw = node.post("/v1/withdrawals", withdrawal(&victim, nonce)).await;
    let declare = node.post("/v1/eligibility", declaration(&newcomer, 0)).await;
    assert_eq!(
        [close.0, withdraw.0, declare.0],
        [StatusCode::OK; 3],
        "another wallet's close-all, withdrawal and declaration: {close:?} {withdraw:?} {declare:?}"
    );

    gate.add_permits(8);
    let statuses: Vec<_> = burst.await.into_iter().map(|(status, _)| status).collect();
    let refused = statuses.iter().filter(|status| **status == StatusCode::TOO_MANY_REQUESTS).count();
    assert_eq!(refused, 6, "the burst past the wallet's share is told to slow down: {statuses:?}");
}

/// Three wallets' new orders from two clients hold every slot outside the reserve: the first client
/// holds its share of four for two wallets, so the third wallet's is refused there and it sends from
/// another. Then a new order is told to retry, while a stop loss, a close and a withdrawal take the
/// reserve.
#[tokio::test]
async fn new_orders_leave_stop_losses_closes_and_withdrawals_the_reserved_slots() {
    let (logs, _guard) = Logs::capture();
    let mut node = Node::start("screening-slot-reserve", true).await;
    production_slots(&mut node);
    let (first, second, third, trader) = (key(0x91), key(0x92), key(0x93), key(0x94));
    for wallet in [&first, &second, &third, &trader] {
        node.onboard(wallet).await;
    }
    let (status, events) = node.post("/v1/orders", order(&trader, 1)).await;
    assert_eq!(status, StatusCode::OK, "{events}");

    let gate = hold(&node, &[&first, &second, &third]);
    let asked = screened(&node);
    let opening = |client, wallet, id| {
        send(&node, client, Method::POST, "/v1/orders", signed_order(wallet, wallet, 1, id, false))
    };
    let mut held = Box::pin(join_all([
        opening(1, &first, 1),
        opening(1, &first, 2),
        opening(1, &second, 1),
        opening(1, &second, 2),
    ]));
    assert!(until_in_flight(&node, &mut held, asked + 4).await.is_none());
    let (status, ..) = opening(1, &third, 1).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "the first client holds its share");
    let mut more = Box::pin(join_all([opening(2, &third, 1), opening(2, &third, 2)]));
    assert!(until_in_flight(&node, &mut more, asked + 6).await.is_none());

    let (status, body, retry_after) = send(&node, 3, Method::POST, "/v1/orders", order(&trader, 2)).await;
    assert_eq!(
        (status, body["error"].as_str(), retry_after),
        (StatusCode::SERVICE_UNAVAILABLE, Some("screening is at capacity"), Some(5))
    );
    let stop = levels_body(&trader, &trader, 2, NOW + 60_000, 1, levels(None, Some("99000")));
    let (status, events, _) = send(&node, 3, Method::POST, "/v1/positions/levels", stop).await;
    let set = of_type(&events, "position_levels_set").len();
    assert_eq!((status, set), (StatusCode::OK, 1), "the stop loss takes a reserved slot: {events}");
    let (status, events, _) =
        send(&node, 3, Method::POST, "/v1/orders", signed_order(&trader, &trader, 3, 2, true)).await;
    assert!(status == StatusCode::OK && fills(&events), "the close takes a reserved slot: {events}");
    let (status, events, _) = send(&node, 3, Method::POST, "/v1/withdrawals", withdrawal(&trader, 4)).await;
    assert_eq!((status, &events[0]["type"]), (StatusCode::OK, &json!("withdrawal_queued")), "{events}");
    assert_eq!(logs.warnings("api_screening_slots_full"), 1, "the operator hears of the refused order");

    gate.add_permits(6);
    let held = held.await.into_iter().chain(more.await).map(|(status, ..)| status).collect::<Vec<_>>();
    assert_eq!(held, [StatusCode::OK; 6]);
}

/// A credited wallet has declared, so the engine could only refuse its declaration: it is refused
/// before any screening. Two copies of one signed order take one slot, so another wallet's order
/// screens beside them with the harness's two slots.
#[tokio::test]
async fn a_repeat_declaration_and_a_copy_of_a_signed_order_hold_no_screening_slot() {
    let node = Node::start("screening-slot-copies", true).await;
    let (wallet, trader, newcomer) = (key(0x91), key(0x93), key(0x94));
    node.onboard(&wallet).await;
    node.onboard(&trader).await;

    let asked = screened(&node);
    let (status, body) = node.post("/v1/eligibility", declaration(&wallet, 1)).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::FORBIDDEN, Some("account has already declared")));
    assert_eq!(screened(&node), asked, "the repeat declaration reaches no source");
    assert_eq!(next_nonce(&node, address(&wallet)).await, 1, "nor uses a nonce");
    let (status, events) = node.post("/v1/eligibility", declaration(&newcomer, 0)).await;
    assert_eq!(status, StatusCode::OK, "{events}");

    let gate = hold(&node, &[&wallet]);
    let asked = screened(&node);
    let mut copies = Box::pin(join_all((0..2).map(|_| node.post("/v1/orders", order(&wallet, 1)))));
    assert!(until_in_flight(&node, &mut copies, asked + 1).await.is_none());
    assert_eq!(screened(&node) - asked, 1, "the copies are screened once");
    let nonce = next_nonce(&node, address(&trader)).await;
    let (status, events) = node.post("/v1/orders", order(&trader, nonce)).await;
    assert_eq!(status, StatusCode::OK, "another wallet's order beside the copies: {events}");

    gate.add_permits(1);
    let answers = copies.await;
    assert!(answers.iter().any(|(status, events)| *status == StatusCode::OK && fills(events)), "{answers:?}");
    let copy = (StatusCode::CONFLICT, json!({"error": "the same request is already being screened"}));
    assert!(answers.contains(&copy), "the copy is answered at once: {answers:?}");
}

/// An order that finds every slot held is told when to retry, and the operator hears of it once while
/// it lasts.
#[tokio::test]
async fn an_order_refused_for_want_of_a_screening_slot_is_told_to_retry_and_raises_an_alert() {
    let (logs, _guard) = Logs::capture();
    let node = Node::start("screening-slots-full", true).await;
    let trader = key(0x93);
    node.onboard(&trader).await;
    let occupied = node.state.access.as_ref().unwrap().api_slots.fill();
    for _ in 0..2 {
        let (status, body, retry_after) = send(&node, 1, Method::POST, "/v1/orders", order(&trader, 1)).await;
        assert_eq!(
            (status, body["error"].as_str(), retry_after),
            (StatusCode::SERVICE_UNAVAILABLE, Some("screening is at capacity"), Some(5))
        );
    }
    assert_eq!(logs.warnings("api_screening_slots_full"), 1, "raised once while orders are refused");
    drop(occupied);
    let (status, events, _) = send(&node, 1, Method::POST, "/v1/orders", order(&trader, 1)).await;
    assert!(status == StatusCode::OK && fills(&events), "a freed slot screens the order: {events}");
}

// ── Helpers ──

/// A node with the production genesis's request rates by tier (2, 5, 10 and 20 a second), the
/// production template's `api.request_bursts` (2 seconds of the rate for the account and 2 for its
/// session keys), buckets that refill in real time as production's do, and production's screening
/// slots.
async fn production_like(name: &str) -> Node {
    let mut genesis = dev_config().genesis;
    let tiers = [(0, 25, 2), (100_000, 50, 5), (400_000, 100, 10), (10_000_000, 250, 20)];
    for (tier, (threshold, orders, rate)) in genesis.params.volume_tiers.iter_mut().zip(tiers) {
        (tier.threshold, tier.day_divisor) = (Fixed::from_int(threshold), 1);
        (tier.max_resting_orders, tier.requests_per_second) = (orders, rate);
    }
    let mut api = dev_config().api;
    api.request_bursts.account = NonZeroU32::new(2).unwrap();
    api.request_bursts.session_key = NonZeroU32::new(2).unwrap();
    let mut node = Node::start_from(name, true, true, false, &genesis).await;
    node.state.limits = Arc::new(ApiLimits::new(&api));
    production_slots(&mut node);
    node
}

/// Gives `node` production's screening slots, shares and reserve, against the same source, with a
/// timeout long enough that a screening the source holds stays in flight however slow the machine.
fn production_slots(node: &mut Node) {
    let address = node.source.lock().unwrap().address;
    let config: AccessSection = toml::from_str(&format!(
        "source='{}'\nendpoint='http://{address}'\ntimeout='60s'\nmax_age='1s'\nmax_response_bytes=4096\n{PRODUCTION_SLOTS}",
        eip712::address(key(0x77).verifying_key()),
    ))
    .unwrap();
    node.state.access = Some(Arc::new(AccessClient::new(&config).unwrap()));
    node.app = router(node.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
}

/// Holds the source's answers about `wallets` until the returned gate gives permits.
fn hold(node: &Node, wallets: &[&SigningKey]) -> Arc<Semaphore> {
    let gate = Arc::new(Semaphore::new(0));
    node.source.lock().unwrap().hold = Some((wallets.iter().map(|wallet| address(wallet)).collect(), gate.clone()));
    gate
}

/// A BTC market order of 0.01 for `account`, signed by `signer` with `nonce` and told apart by
/// `client_id`: a buy, or with `close` a reduce-only sell.
fn signed_order(account: &SigningKey, signer: &SigningKey, nonce: u64, client_id: u64, close: bool) -> Value {
    let mut request: OrderRequest = serde_json::from_value(order(account, nonce)).unwrap();
    (request.client_id, request.reduce_only) = (client_id, close);
    if close {
        request.side = Side::Sell;
    }
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, nonce, NOW + 60_000);
    request.auth = Some(RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(signer, &hash) });
    json!(request)
}

/// Whether `events` hold a fill.
fn fills(events: &Value) -> bool {
    events.as_array().is_some_and(|events| events.iter().any(|event| event["type"] == "fill"))
}

/// Polls `requests` until the source has been asked at least `first` times in all and no further
/// request has reached it for 300 ms, so every one of them the node admitted is waiting on the
/// source. Returns the answers instead if they all come first.
async fn until_in_flight<T>(node: &Node, requests: &mut Pin<Box<impl Future<Output = T>>>, first: usize) -> Option<T> {
    tokio::select! {
        answers = requests => Some(answers),
        () = async {
            while screened(node) < first {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let mut asked = screened(node);
            loop {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if screened(node) == asked {
                    break;
                }
                asked = screened(node);
            }
        } => None,
    }
}
