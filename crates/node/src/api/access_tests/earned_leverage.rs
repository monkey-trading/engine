//! The `V64` dev scenario: a tier's margin floor caps the leverage a wallet opens at and the cash it
//! can withdraw, through the signed order and withdrawal routes and the account read on deterministic
//! time. The node's tier table is dev's with the production genesis's margin floors, 5% at tier 0 and
//! 2% at tier 1, and thresholds a few fills reach in a day ($100k counted for tier 1, $400k for tier
//! 2). A fresh wallet holding $10,000 is
//! refused a BTC-USD short at 25x and opens 19.8x; at tier 1 it opens 50x, and at tier 2 100x. Thirty
//! days on its tier falls: the short stays open, a withdrawal stops at the new initial requirement, and
//! BTC rising closes it out only at the market's bands. The public log replays to the same state, and a
//! restart recovers it.

use engine_core::types::MarketId;
use engine_core::volume_tier::{DAY_MS, MIN_HOLD_MS, day_of};

use super::spreads::{fill_event, fund, order_events, price};
use super::*;
use crate::clock::Clock;

const BTC: MarketId = 1;

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_a_tiers_margin_floor_caps_leverage_and_withdrawals() {
    let mut genesis = dev_config().genesis;
    let tiers = [(0, 500), (100_000, 200), (400_000, 0), (10_000_000, 0)];
    for (tier, (threshold, floor)) in genesis.params.volume_tiers.iter_mut().zip(tiers) {
        (tier.threshold, tier.day_divisor, tier.margin_floor_bps) = (Fixed::from_int(threshold), 1, floor);
    }
    let mut node = Node::start_from("earned-leverage-dev-scenario", true, true, false, &genesis).await;
    let a = key(0x83);
    let wallet = eip712::address(a.verifying_key());
    node.onboard(&a).await;
    let events = withdraw(&node, &a, 90_000).await;
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "withdrawal_queued"), "{events}");

    // 1. A fresh wallet holding $10,000 is refused a $250k short (25x), which BTC's 1% admits but tier
    // 0's 5% floor does not, and opens $198k (19.8x); the read gives 20x as its most.
    price(&node, BTC, "100000").await;
    assert_eq!(refused_at(&node, &a, "2.5").await, "12500");
    fill_event(&node, &a, BTC, Side::Sell, "1.98", "100000").await;
    let read = node.get(&format!("/v1/accounts/{wallet}")).await;
    assert_eq!((&read["initial_margin"], &read["max_leverage"][BTC.to_string()]), (&json!("9900"), &json!("20")));
    assert_eq!((&read["margin"], &read["maintenance_margin"]), (&json!("1980"), &json!("990")), "{read}");

    // 2. Held 60 s, the $198k reaches tier 1: $498k (50x of $9,960) is refused, $488k opens.
    wait(&node, MIN_HOLD_MS as u64).await;
    price(&node, BTC, "100000").await;
    assert_eq!(leverage(&node, wallet).await, (1, json!("50")));
    assert_eq!(refused_at(&node, &a, "3").await, "9960");
    fill_event(&node, &a, BTC, Side::Sell, "2.9", "100000").await;

    // 3. Held 60 s, $488k reaches tier 2, which has no floor: $978k (about 100x) opens at BTC's 1%.
    wait(&node, MIN_HOLD_MS as u64).await;
    price(&node, BTC, "100000").await;
    assert_eq!(leverage(&node, wallet).await, (2, json!("100")));
    assert_eq!(refused_at(&node, &a, "5").await, "9880");
    fill_event(&node, &a, BTC, Side::Sell, "4.9", "100000").await;
    let read = node.get(&format!("/v1/accounts/{wallet}")).await;
    assert_eq!((&read["initial_margin"], &read["margin"]), (&json!("9780"), &json!("9780")), "{read}");

    // 4. Thirty days on, the counted volume has left the window and the wallet is at tier 0 again. Its
    // short stays open: tier 0's 5% puts its initial requirement at $48,900, above its equity, so it
    // can withdraw nothing, yet nothing is closed out.
    wait_until(&node, (day_of(node.clock.now_ms()) + 30) * DAY_MS).await;
    let events = price(&node, BTC, "100000").await;
    assert!(!events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    assert_eq!(leverage(&node, wallet).await, (0, json!("20")));
    let read = node.get(&format!("/v1/accounts/{wallet}")).await;
    assert_eq!((&read["initial_margin"], &read["withdrawable"]), (&json!("48900"), &json!("0")), "{read}");
    assert_eq!(withdraw(&node, &a, 10).await[0]["reason"]["available"], "0");
    // With $45,000 more it withdraws what its equity holds above the requirement, and not a dollar more.
    fund(&node, &a, 45_000).await;
    let read = node.get(&format!("/v1/accounts/{wallet}")).await;
    let free: Fixed = read["withdrawable"].as_str().unwrap().parse().unwrap();
    assert!(free > Fixed::from_int(4_000) && free < Fixed::from_int(10_000), "{read}");
    let whole = i64::try_from(free.raw() / Fixed::ONE.raw()).unwrap();
    let events = withdraw(&node, &a, whole + 1).await;
    assert_eq!(
        (events[0]["reason"]["kind"].as_str(), &events[0]["reason"]["available"]),
        (Some("insufficient_funds"), &read["withdrawable"])
    );
    let events = withdraw(&node, &a, whole).await;
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "withdrawal_queued"), "{events}");

    // 5. BTC rising to $102,000 takes its equity far below the initial requirement but not below the
    // close-out level of BTC's 1%; at $105,000 the short is closed out.
    wait(&node, 1_000).await;
    let events = price(&node, BTC, "102000").await;
    assert!(!events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    let read = node.get(&format!("/v1/accounts/{wallet}")).await;
    let amount = |key: &str| read[key].as_str().unwrap().parse::<Fixed>().unwrap();
    assert!(amount("equity") < amount("initial_margin") && amount("equity") > amount("maintenance_margin"), "{read}");
    wait(&node, 1_000).await;
    let events = price(&node, BTC, "105000").await;
    let closed =
        events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.liquidation && fill.account == wallet));
    assert!(closed, "{events:?}");

    // 6. The public log, the signed withdrawals included, replays against the enclave's key to the same
    // state, and a restart recovers the root it replays to.
    let domain = crate::log::CheckpointDomain { chain_id: 31_337, vault: VAULT };
    let signer = AccountId(EnclaveKey::test_key().address());
    let mut replayed = replay_with_signer(&node.entries(), domain, signer).unwrap().engine;
    let live =
        node.state.query(move |engine| (engine.accounts()[&wallet].clone(), engine.margin_standing(wallet))).await;
    assert_eq!(live.unwrap(), (replayed.accounts()[&wallet].clone(), replayed.margin_standing(wallet)));
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

/// The initial requirement `client`'s BTC short of `qty` at $100,000 is refused below.
async fn refused_at(node: &Node, client: &SigningKey, qty: &str) -> String {
    let events = order_events(node, client, BTC, Side::Sell, qty, "100000").await;
    let refused = events.as_array().unwrap().iter().find(|event| event["type"] == "order_rejected");
    let reason = &refused.unwrap_or_else(|| panic!("{events}"))["reason"];
    assert_eq!(reason["kind"], "insufficient_margin", "{events}");
    reason["required"].as_str().unwrap().to_owned()
}

/// `account`'s volume tier and BTC's maximum leverage, from the account read.
async fn leverage(node: &Node, account: AccountId) -> (u64, Value) {
    let read = node.get(&format!("/v1/accounts/{account}")).await;
    (read["volume"]["tier"].as_u64().unwrap(), read["max_leverage"][BTC.to_string()].clone())
}

/// `client`'s signed withdrawal of `amount` dollars through the withdrawal route; the events it gives.
async fn withdraw(node: &Node, client: &SigningKey, amount: i64) -> Value {
    let account = eip712::address(client.verifying_key());
    let nonce = node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap();
    let (amount, expires_at) = (Fixed::from_int(amount), node.clock.now_ms() + 60_000);
    let hash = eip712::withdrawal_hash(&eip712::domain(31_337, VAULT), account, amount, nonce, expires_at);
    let auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(client, &hash) });
    let (status, events) = node.post("/v1/withdrawals", json!(WithdrawalRequest { account, amount, auth })).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    events
}

/// Moves the node's time forward by `ms`, with the screening source observing at the new time.
pub(super) async fn wait(node: &Node, ms: u64) {
    node.move_time(ms).await;
    node.source.lock().unwrap().observed_at = node.clock.now_ms();
}

/// Moves the node's time forward to `ts`.
pub(super) async fn wait_until(node: &Node, ts: i64) {
    wait(node, u64::try_from(ts - node.clock.now_ms()).expect("forward")).await;
}
