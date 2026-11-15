//! The `V66` dev scenario: a tier's limits cap the orders a wallet rests and the signed requests the
//! node admits from it, through the signed order route and the account read on deterministic time. The
//! node's tier table is dev's with the production genesis's limits (25, 50, 100 and 250 resting orders,
//! 2, 5, 10 and 20 requests a second) and thresholds a few fills reach in a day, and an account's
//! bucket holds 13 seconds of its tier's rate (the test limiter never refills, so it holds exactly
//! that). A
//! fresh wallet rests 25 orders and the 26th is refused naming tier 0; the 27th request and the ten
//! signed orders after it are refused with 429 naming tier 0's rate. A wallet that has reached tier 2
//! sends the same requests and has them accepted, and rests 30 orders. Thirty days on its tier falls:
//! its 30 orders stay and a new one is refused at tier 0's 25. The public log replays to the same state,
//! and a restart recovers it. A second test holds a wallet's session keys to the tier's rate in a
//! bucket of their own.

use std::num::NonZeroU32;

use engine_core::types::MarketId;
use engine_core::volume_tier::{DAY_MS, MIN_HOLD_MS, day_of};

use super::earned_leverage::{wait, wait_until};
use super::session_keys::{registration, session_order};
use super::spreads::{fill_event, price};
use super::*;
use crate::clock::Clock;

const BTC: MarketId = 1;

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_a_tiers_limits_cap_resting_orders_and_requests() {
    let (mut node, genesis) = tier_node("tier-limits-dev-scenario", 13, 2).await;
    let (a, b) = (key(0x84), key(0x85));
    let (wallet_a, wallet_b) = (eip712::address(a.verifying_key()), eip712::address(b.verifying_key()));
    node.onboard(&a).await;
    node.onboard(&b).await;
    price(&node, BTC, "100000").await;

    // 1. A fresh wallet is at tier 0: its read gives 25 orders and 2 requests a second, it rests 25
    // orders, and the 26th is refused naming tier 0 and its limit.
    let read = node.get(&format!("/v1/accounts/{wallet_a}")).await;
    assert_eq!(
        (&read["volume"]["max_resting_orders"], &read["volume"]["requests_per_second"]),
        (&json!(25), &json!(2))
    );
    for placed in 1..=25 {
        let (status, events) = bid(&node, &a).await;
        assert_eq!(status, StatusCode::OK, "{placed}: {events}");
        assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "order_accepted"), "{events}");
    }
    let (status, events) = bid(&node, &a).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let reason = &events[0]["reason"];
    assert_eq!(
        (&events[0]["type"], &reason["kind"], &reason["tier"], &reason["max"]),
        (&json!("order_rejected"), &json!("tier_order_limit"), &json!(0), &json!(25))
    );

    // 2. Its bucket, 26 requests at 2 a second over 13 seconds, is empty: the next request and the ten
    // signed orders after it get 429 naming tier 0's rate, and use no nonce.
    let nonce = node.state.query(move |engine| engine.next_request_nonce(wallet_a)).await.unwrap();
    for _ in 0..11 {
        let (status, body) = bid(&node, &a).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert_eq!(body, json!({ "error": "rate limited", "tier": 0, "requests_per_second": 2 }));
    }
    assert_eq!(node.state.query(move |engine| engine.next_request_nonce(wallet_a)).await.unwrap(), nonce);

    // 3. A wallet that opens $490k of BTC and holds it 60 s reaches tier 2, whose read gives 100 orders
    // and 10 requests a second; its 130-request bucket takes the same ten signed orders, and 20 more.
    fill_event(&node, &b, BTC, Side::Sell, "4.9", "100000").await;
    wait(&node, MIN_HOLD_MS as u64).await;
    price(&node, BTC, "100000").await;
    let read = node.get(&format!("/v1/accounts/{wallet_b}")).await;
    assert_eq!(read["volume"]["tier"], 2);
    assert_eq!(
        (&read["volume"]["max_resting_orders"], &read["volume"]["requests_per_second"]),
        (&json!(100), &json!(10))
    );
    for placed in 1..=30 {
        let (status, events) = bid(&node, &b).await;
        assert_eq!(status, StatusCode::OK, "{placed}: {events}");
        assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "order_accepted"), "{events}");
    }

    // 4. Thirty days on, the counted volume has left the window and tier 2's limits with it. The 30
    // orders stay, the read gives tier 0's limits, and a new order is refused at 25.
    wait_until(&node, (day_of(node.clock.now_ms()) + 30) * DAY_MS).await;
    price(&node, BTC, "100000").await;
    let read = node.get(&format!("/v1/accounts/{wallet_b}")).await;
    assert_eq!(read["volume"]["tier"], 0);
    assert_eq!(
        (&read["volume"]["max_resting_orders"], &read["volume"]["requests_per_second"]),
        (&json!(25), &json!(2))
    );
    assert_eq!(read["orders"].as_object().unwrap().len(), 30);
    let (status, events) = bid(&node, &b).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let reason = &events[0]["reason"];
    assert_eq!((&reason["kind"], &reason["tier"], &reason["max"]), (&json!("tier_order_limit"), &json!(0), &json!(25)));
    let read = node.get(&format!("/v1/accounts/{wallet_b}")).await;
    assert_eq!(read["orders"].as_object().unwrap().len(), 30);

    // 5. The public log replays against the enclave's key to the same state, and a restart recovers the
    // root it replays to.
    let domain = crate::log::CheckpointDomain { chain_id: 31_337, vault: VAULT };
    let signer = AccountId(EnclaveKey::test_key().address());
    let mut replayed = replay_with_signer(&node.entries(), domain, signer).unwrap().engine;
    let live = node
        .state
        .query(move |engine| {
            ([wallet_a, wallet_b].map(|account| engine.accounts()[&account].clone()), engine.request_limit(wallet_b))
        })
        .await
        .unwrap();
    let replay_accounts = [wallet_a, wallet_b].map(|account| replayed.accounts()[&account].clone());
    assert_eq!(live, (replay_accounts, replayed.request_limit(wallet_b)));
    let policy = node.state.query(|engine| engine.access_policy().cloned()).await.unwrap();
    let (operator, clock, dir) = (node.state.operator, node.clock.clone(), node.dir.clone());
    let sequencer = node.tasks.remove(1);
    sequencer.abort();
    let _ = sequencer.await;
    drop(node);
    let cfg = crate::sequencer::SequencerConfig {
        access_policy: policy,
        chain_id: 31_337,
        vault: VAULT,
        operator,
        clock,
        genesis: crate::sequencer::genesis_commands(&genesis),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let enclave = Arc::new(EnclaveKey::test_key());
    let mut recovered =
        Sequencer::recover(cfg, enclave, broadcast::channel(16).0, mpsc::unbounded_channel().0).unwrap();
    assert_eq!(recovered.state_root(), replayed.state_root());
}

/// `client`'s signed buy limit of 0.01 BTC at $50,000, well below the market, through the order route:
/// its status and body.
async fn bid(node: &Node, client: &SigningKey) -> (StatusCode, Value) {
    let account = eip712::address(client.verifying_key());
    let nonce = node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap();
    let expires_at = node.clock.now_ms() + 60_000;
    let mut request = OrderRequest {
        account,
        client_id: nonce,
        market: BTC,
        side: Side::Buy,
        qty: "0.01".parse().unwrap(),
        kind: OrderKind::Limit { price: Fixed::from_int(50_000) },
        reduce_only: false,
        auth: None,
    };
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(client, &hash) });
    node.post("/v1/orders", json!(request)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wallets_session_keys_are_held_to_its_tiers_rate_in_a_bucket_apart_from_its_own() {
    // At tier 0's 2 a second, the account's bucket holds 4 seconds (8 requests) and its keys' 2 (4).
    let (node, _) = tier_node("tier-limits-session-keys", 4, 2).await;
    let (account, session, other) = (key(0x86), key(7), key(8));
    node.onboard(&account).await;
    price(&node, BTC, "100000").await;
    for (nonce, key) in [(1, &session), (2, &other)] {
        let (status, events) = node.post("/v1/session-keys", registration(&account, key, nonce, NOW + 3_600_000)).await;
        assert_eq!(status, StatusCode::OK, "{events}");
    }
    // Four orders signed by either key use the keys' shared bucket; the fifth gets 429 naming tier 0.
    let mut nonces = [0u64; 2];
    for index in 0..4 {
        let slot = index % 2;
        let (status, events) =
            node.post("/v1/orders", session_order(&account, [&session, &other][slot], nonces[slot])).await;
        assert_eq!(status, StatusCode::OK, "{index}: {events}");
        nonces[slot] += 1;
    }
    let (status, body) = node.post("/v1/orders", session_order(&account, &session, nonces[0])).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body, json!({ "error": "rate limited", "tier": 0, "requests_per_second": 2 }));
    // The wallet's own bucket, less the two registrations, is untouched: six requests, and then 429.
    for placed in 1..=6 {
        let (status, events) = bid(&node, &account).await;
        assert_eq!(status, StatusCode::OK, "{placed}: {events}");
    }
    let (status, body) = bid(&node, &account).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["tier"], 0);
}

/// A node whose tier table is dev's with the production genesis's limits and thresholds a few fills
/// reach in a day, its buckets (which refill once an hour) holding `account_seconds` and `session_seconds`
/// of the tier's rate; and the genesis it started from.
async fn tier_node(name: &str, account_seconds: u32, session_seconds: u32) -> (Node, Genesis) {
    let mut genesis = dev_config().genesis;
    let tiers = [(0, 25, 2), (100_000, 50, 5), (400_000, 100, 10), (10_000_000, 250, 20)];
    for (tier, (threshold, orders, rate)) in genesis.params.volume_tiers.iter_mut().zip(tiers) {
        (tier.threshold, tier.day_divisor) = (Fixed::from_int(threshold), 1);
        (tier.max_resting_orders, tier.requests_per_second) = (orders, rate);
    }
    let mut api = dev_config().api;
    api.request_bursts.account = NonZeroU32::new(account_seconds).unwrap();
    api.request_bursts.session_key = NonZeroU32::new(session_seconds).unwrap();
    let mut node = Node::start_from(name, true, true, false, &genesis).await;
    node.state.limits = Arc::new(ApiLimits::with_quota(
        &api,
        |rate: RateLimit| Quota::with_period(Duration::from_secs(3600)).unwrap().allow_burst(rate.burst),
        DefaultClock::default(),
    ));
    node.app = router(node.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
    (node, genesis)
}
