//! The `V59` dev scenario: a fresh wallet trades at the best price only up to a quarter of BTC-USD's
//! first band, its volume counts only once an opening has been held 60 s and never for churn or a
//! round trip, each day counts at most half of tier 1's $1M, and the tier it reaches on its third day
//! moves its first band to $1M, through the signed order route and the account read on deterministic
//! time; the public log replays to the same state, and a restart recovers it.

use engine_core::types::MarketId;
use engine_core::volume_tier::{DAY_MS, day_of};

use super::spreads::{fill_event, fund, price};
use super::*;
use crate::clock::Clock;

const BTC: MarketId = 1;

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_counted_volume_earns_a_tier_and_more_size_at_the_best_price() {
    let mut node = Node::start("volume-tiers-dev-scenario", true).await;
    let (a, b) = (key(0x81), key(0x82));
    for client in [&a, &b] {
        node.onboard(client).await;
        fund(&node, client, 1_000_000).await;
    }
    let wallet = eip712::address(a.verifying_key());
    let day_one = day_of(node.clock.now_ms()) + 1;
    let midnight = (day_one + 1) * DAY_MS;

    // 1. Day one, five minutes before midnight: churn closed after 30 s, and a round trip closed at
    // its index a second later, count nothing on either fill.
    wait_until(&node, midnight - 300_000).await;
    price(&node, BTC, "100000").await;
    fill_event(&node, &a, BTC, Side::Buy, "1", "100000").await;
    wait(&node, 30_000).await;
    price(&node, BTC, "100100").await;
    fill_event(&node, &a, BTC, Side::Sell, "1", "100100").await;
    wait(&node, 30_000).await;
    price(&node, BTC, "100000").await;
    fill_event(&node, &a, BTC, Side::Buy, "1", "100000").await;
    wait(&node, 1_000).await;
    price(&node, BTC, "100000").await;
    fill_event(&node, &a, BTC, Side::Sell, "1", "100000").await;
    assert_eq!(volume(&node, wallet).await, (0, Fixed::ZERO, Fixed::ZERO));

    // 2. 30 s before midnight, B opens a $600k short and the wallet the $600k long it offsets. At tier
    // 0 its first band ends at a quarter of BTC's $2M: 2bp to $500k and 4bp on the $100k above.
    wait_until(&node, midnight - 30_000).await;
    price(&node, BTC, "100000").await;
    fill_event(&node, &b, BTC, Side::Sell, "6", "100000").await;
    let long = fill_event(&node, &a, BTC, Side::Buy, "6", "100000").await;
    assert_eq!(long["price"], "100023.333333334", "{long}");
    let opened: Fixed = "600140.000000004".parse().unwrap();
    assert_eq!(volume(&node, wallet).await, (0, Fixed::ZERO, opened));
    let account = node.get(&format!("/v1/accounts/{wallet}")).await;
    assert_eq!(account["volume"]["best_price_bounds"][BTC.to_string()], "500000", "{account}");

    // 3. It crosses midnight before it matures, and counts on day two once held 60 s.
    wait_until(&node, midnight + 29_999).await;
    price(&node, BTC, "100000").await;
    assert_eq!(volume(&node, wallet).await, (0, Fixed::ZERO, opened));
    wait(&node, 1).await;
    price(&node, BTC, "100000").await;
    assert_eq!(volume(&node, wallet).await, (0, opened, Fixed::ZERO));

    // 4. Day two: a close reopened at its index a second later is a round trip, taken back whole; the
    // book holds the midnight open on day two.
    wait_until(&node, midnight + DAY_MS / 2).await;
    price(&node, BTC, "100000").await;
    fill_event(&node, &a, BTC, Side::Sell, "1", "100000").await;
    wait(&node, 1_000).await;
    price(&node, BTC, "100000").await;
    fill_event(&node, &a, BTC, Side::Buy, "1", "100000").await;
    wait(&node, 10_000).await;
    price(&node, BTC, "100000").await;
    assert_eq!(volume(&node, wallet).await, (0, opened, Fixed::ZERO));
    let days = node.state.query(move |engine| engine.account_volume(wallet).unwrap().days.clone()).await.unwrap();
    assert_eq!((days.len(), days[0].day, days[0].volume), (1, day_one + 1, opened), "{days:?}");

    // 5. Day three: the wallet closes its $600k. While the close is pending it stays at tier 0, and
    // once the round-trip horizon passes, $500k from each of two days reaches tier 1.
    wait_until(&node, midnight + DAY_MS + DAY_MS / 2).await;
    price(&node, BTC, "100000").await;
    let close = fill_event(&node, &a, BTC, Side::Sell, "6", "100000").await;
    let closed = Fixed::from_int(6).mul(close["price"].as_str().unwrap().parse().unwrap());
    assert_eq!(volume(&node, wallet).await, (0, opened, closed));
    let account = node.get(&format!("/v1/accounts/{wallet}")).await;
    assert_eq!(account["volume"]["best_price_bounds"][BTC.to_string()], "500000", "{account}");
    wait(&node, 5_000).await;
    price(&node, BTC, "100000").await;
    assert_eq!(volume(&node, wallet).await, (1, opened + closed, Fixed::ZERO));
    let account = node.get(&format!("/v1/accounts/{wallet}")).await;
    let progress = &account["volume"]["progress"][1];
    assert_eq!((progress["capped"].as_str(), progress["days"].as_u64()), (Some("1000000"), Some(2)), "{account}");
    assert_eq!(account["volume"]["best_price_bounds"][BTC.to_string()], "1000000", "{account}");
    // Tier 1 trades $1M at the best price, 2bp on all of it.
    let buy = fill_event(&node, &a, BTC, Side::Buy, "10", "100000").await;
    assert_eq!(buy["price"], "100020", "{buy}");
    let house = node.get("/v1/house/state").await;
    assert_eq!(house["params"]["volume_tiers"][1]["threshold"], "1000000", "{house}");

    // 6. The public log replays to the same state, and a restart recovers the root it replays to.
    let accounts = [&a, &b].map(|client| eip712::address(client.verifying_key()));
    let mut replayed = replay(&node.entries()).unwrap().engine;
    let live = node
        .state
        .query(move |engine| accounts.map(|id| (engine.account_volume(id).cloned(), engine.volume_standing(id))))
        .await
        .unwrap();
    assert_eq!(accounts.map(|id| (replayed.account_volume(id).cloned(), replayed.volume_standing(id))), live);
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
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let enclave = Arc::new(EnclaveKey::test_key());
    let mut recovered =
        Sequencer::recover(cfg, enclave, broadcast::channel(16).0, mpsc::unbounded_channel().0).unwrap();
    assert_eq!(recovered.state_root(), replayed.state_root());
}

/// `account`'s tier, counted volume and pending volume, from the account read.
async fn volume(node: &Node, account: AccountId) -> (u64, Fixed, Fixed) {
    let read = node.get(&format!("/v1/accounts/{account}")).await;
    let volume = &read["volume"];
    let amount = |key: &str| -> Fixed { volume[key].as_str().unwrap().parse().unwrap() };
    (volume["tier"].as_u64().unwrap(), amount("matured"), amount("pending"))
}

/// Moves the node's time forward by `ms`, with the screening source observing at the new time.
async fn wait(node: &Node, ms: u64) {
    node.move_time(ms).await;
    node.source.lock().unwrap().observed_at = node.clock.now_ms();
}

/// Moves the node's time forward to `ts`.
async fn wait_until(node: &Node, ts: i64) {
    wait(node, u64::try_from(ts - node.clock.now_ms()).expect("forward")).await;
}
