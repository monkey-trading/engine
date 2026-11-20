//! The `V79` dev scenario: take-profit and stop-loss levels on a dev node, through the signed order,
//! levels, cancel-all and withdrawal routes, the account and market reads, the history, and the
//! operator's stress floor over TLS, on deterministic time with BTC-USD's index starting at 100,000.
//! BTC's minimum level distance is 30 bps and a fresh wallet's close of up to $500k pays its 2 bp first
//! band, so a long closes at the index less 2 bps. The node's tier table is dev's with the production
//! genesis's resting-order limits (25 at tier 0), and its request buckets hold more than the scenario
//! sends.
//!
//! 1. A opens a long naming a take profit and a stop loss; a stop loss inside 30 bps of its closing
//!    price, and one above it, are refused naming both.
//! 2. Its levels stay through an add and a partial close; Cancel all cancels a resting limit order and
//!    leaves them.
//! 3. One price far below the stop loss fills the whole position at its closing price, below the level,
//!    and removes the take profit with it.
//! 4. B's stop loss, just inside the band, fires when the operator raises BTC's stress floor to level 1
//!    and the next price leaves the index where it was.
//! 5. C's take profit fills at its level or better once its closing price, not the index, reaches it.
//! 6. D's resting buy limit names both levels, which attach to the long its fill opens.
//! 7. E's stop loss above its close-out level fills on a price past both, and no close-out runs.
//! 8. F's levels go when it reverses its short, and G's when it is closed out for margin, each naming why.
//! 9. A tier-0 wallet resting 24 orders sets levels on one position in its 25th slot, is refused them on a
//!    second naming tier 0, and moves the first position's stop loss.
//! 10. A WTI long's stop loss, held over the weekend closure, fires on nothing while the market is shut
//!     and fills at the first price after the reopening, below it.
//! 11. The history serves the levels' events by kind; the public log replays to the same state and
//!     events, and a restarted node recovers it and reports the same levels.

use std::num::NonZeroU32;

use engine_core::command::{AdminCommand, CancelAllRequest};
use engine_core::event::Fill;
use engine_core::levels::{LevelKind, LevelsRemoved, PositionLevels};
use engine_core::types::MarketId;
use engine_core::volume_tier::ACTIVATION_MAX_RESTING_ORDERS;
use tokio::time::timeout;

use super::earned_leverage::{wait, wait_until, withdraw};
use super::levels::{levels, market, of_type, order, set_levels};
use super::spreads::{Operator, price};
use super::weekend_margin::{CLOSE, REOPEN, WTI};
use super::*;
use crate::api::Feed;
use crate::clock::Clock;
use crate::config::HistorySection;
use crate::history::derived::Deriver;
use crate::test_support::{LOADED_WAIT, RETENTION, open_history};

const BTC: MarketId = 1;
const ETH: MarketId = 2;
/// Past BTC's one-minute move window, so the next price is measured from itself and raises no stress.
const PAST_THE_MOVE_WINDOW_MS: u64 = 70_000;

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_levels_close_a_whole_position_at_its_closing_price_and_follow_it() {
    let (mut node, genesis) = levels_node().await;
    let operator = Operator::serving(&node).await;
    let wallets: [SigningKey; 9] = std::array::from_fn(|n| key(0x91 + u8::try_from(n).unwrap()));
    for wallet in &wallets {
        node.onboard(wallet).await;
    }
    let [a, b, c, d, e, f, g, h, w] = &wallets;

    // 1. A opens 1 BTC naming a take profit 2% above the index and a stop loss 2% below it. Its closing
    // price is 99,980: a stop loss inside BTC's 30 bps of it, and one above it, are refused naming both.
    price(&node, BTC, "100000").await;
    let named_a = levels(Some("102000"), Some("98000"));
    let events = accepted(order(&node, a, BTC, Side::Buy, "1", market("100000"), named_a).await);
    assert_eq!(of_type(&events, "position_levels_set").len(), 1, "{events}");
    let held_a = json!({ "1": { "take_profit": "102000", "stop_loss": "98000" } });
    assert_eq!(read(&node, a).await["levels"], held_a);
    let a_id = id(a);
    let quote = node.state.query(move |engine| engine.quote(a_id, BTC, Side::Sell, Fixed::ONE)).await.unwrap();
    assert_eq!(quote.unwrap().price, fixed("99980"), "the closing price is what selling the position pays");
    for stop_loss in ["99800", "100500"] {
        let events = accepted(set_levels(&node, a, a, BTC, levels(Some("102000"), Some(stop_loss))).await);
        let reason =
            json!({ "kind": "level_too_close", "level": stop_loss, "min_distance_bps": 30, "reference": "99980" });
        assert_eq!((&events[0]["type"], &events[0]["reason"]), (&json!("order_rejected"), &reason), "{events}");
    }
    assert_eq!(read(&node, a).await["levels"], held_a);

    // 2. Both levels stay through an add and a partial close. Cancel all cancels a resting limit order and
    // leaves them.
    accepted(order(&node, a, BTC, Side::Buy, "1", market("100000"), levels(None, None)).await);
    accepted(order(&node, a, BTC, Side::Sell, "0.5", market("100000"), levels(None, None)).await);
    let read_a = read(&node, a).await;
    assert_eq!((&read_a["positions"]["1"]["qty"], &read_a["levels"]), (&json!("1.5"), &held_a), "{read_a}");
    let limit = OrderKind::Limit { price: Fixed::from_int(90_000) };
    let events = accepted(order(&node, a, BTC, Side::Buy, "0.1", limit, levels(None, None)).await);
    assert_eq!(of_type(&events, "order_accepted").len(), 1, "{events}");
    assert_eq!(read(&node, a).await["orders"].as_object().unwrap().len(), 1);
    let events = accepted(cancel_all(&node, a).await);
    assert_eq!(of_type(&events, "order_cancelled").len(), 1, "{events}");
    let read_a = read(&node, a).await;
    assert!(read_a["orders"].as_object().unwrap().is_empty() && read_a["levels"] == held_a, "{read_a}");

    // 3. One price at 95,000 fills the whole 1.5 at its closing price there, 94,981, below the stop loss,
    // and removes the take profit with it.
    wait(&node, PAST_THE_MOVE_WINDOW_MS).await;
    let events = price(&node, BTC, "95000").await;
    let fill = fill_of(&events, a);
    assert_eq!(
        (fill.side, fill.qty, fill.price, fill.position),
        (Side::Sell, fixed("1.5"), fixed("94981"), Fixed::ZERO)
    );
    assert!(events.contains(&triggered(a, BTC, LevelKind::StopLoss, "98000", fill.order_id)), "{events:?}");
    assert!(events.contains(&removed(a, BTC, named_a, LevelsRemoved::StopLoss)), "{events:?}");
    let read_a = read(&node, a).await;
    assert!(read_a["positions"].as_object().unwrap().is_empty() && read_a.get("levels").is_none(), "{read_a}");

    // 4. B's stop loss at 99,500, set beyond the distance, is just inside the band once the index has eased
    // to 99,540: B's closing price there, 99,520.09, is above it. The operator raises BTC's stress floor to
    // level 1, a 6 bp close, and the next price, the index where it was, fills it at 99,480.276.
    wait(&node, PAST_THE_MOVE_WINDOW_MS).await;
    price(&node, BTC, "100000").await;
    accepted(order(&node, b, BTC, Side::Buy, "1", market("100000"), levels(None, Some("99500"))).await);
    wait(&node, 1_000).await;
    assert!(!fired(&price(&node, BTC, "99540").await));
    operator.applies(&node, AdminCommand::SetStressFloor { market: Some(BTC), level: 1 }).await;
    assert_eq!(listed(&node, BTC).await["stress_level"], 1);
    assert_eq!(read(&node, b).await["levels"], json!({ "1": { "stop_loss": "99500" } }));
    wait(&node, 1_000).await;
    let events = price(&node, BTC, "99540").await;
    let fill = fill_of(&events, b);
    assert_eq!((fill.index, fill.price, fill.position), (fixed("99540"), fixed("99480.276"), Fixed::ZERO));
    assert!(events.contains(&triggered(b, BTC, LevelKind::StopLoss, "99500", fill.order_id)), "{events:?}");
    operator.applies(&node, AdminCommand::SetStressFloor { market: Some(BTC), level: 0 }).await;
    assert_eq!(listed(&node, BTC).await["stress_level"], 0);

    // 5. C's take profit at 101,000 does not fill when the index reaches it, as its closing price is 2 bps
    // short; at 101,030 the closing price is 101,009.794, and it fills there, at its level or better.
    wait(&node, PAST_THE_MOVE_WINDOW_MS).await;
    price(&node, BTC, "100000").await;
    accepted(order(&node, c, BTC, Side::Buy, "1", market("100000"), levels(Some("101000"), None)).await);
    wait(&node, 1_000).await;
    assert!(!fired(&price(&node, BTC, "101000").await));
    wait(&node, 1_000).await;
    let events = price(&node, BTC, "101030").await;
    let fill = fill_of(&events, c);
    assert_eq!((fill.price, fill.position), (fixed("101009.794"), Fixed::ZERO), "{events:?}");
    assert!(events.contains(&triggered(c, BTC, LevelKind::TakeProfit, "101000", fill.order_id)), "{events:?}");
    let named_c = levels(Some("101000"), None);
    assert!(events.contains(&removed(c, BTC, named_c, LevelsRemoved::TakeProfit)), "{events:?}");

    // 6. D rests a buy limit at 100,000 naming both levels, each the distance beyond its price; the index
    // falling to 99,900 fills it, and the levels attach to the new long.
    let named_d = levels(Some("102000"), Some("98000"));
    let limit = OrderKind::Limit { price: Fixed::from_int(100_000) };
    accepted(order(&node, d, BTC, Side::Buy, "1", limit, named_d).await);
    let read_d = read(&node, d).await;
    let resting = read_d["orders"].as_object().unwrap().values().next().unwrap().clone();
    assert_eq!((&resting["take_profit"], &resting["stop_loss"]), (&json!("102000"), &json!("98000")), "{read_d}");
    assert!(read_d.get("levels").is_none(), "{read_d}");
    wait(&node, 1_000).await;
    let events = price(&node, BTC, "99900").await;
    let fill = fill_of(&events, d);
    assert!(fill.price <= fixed("100000") && fill.position == Fixed::ONE, "{events:?}");
    assert!(events.contains(&set_event(d, BTC, named_d)), "{events:?}");
    let held_d = json!({ "1": { "take_profit": "102000", "stop_loss": "98000" } });
    let read_d = read(&node, d).await;
    assert!(read_d["orders"].as_object().unwrap().is_empty() && read_d["levels"] == held_d, "{read_d}");

    // 7. E, holding $10,000, opens 9 BTC with a stop loss at 99,600, above where the account read says it
    // closes out. One price at 99,000 passes both: the stop loss fills, and no close-out runs.
    assert_eq!(of_type(&withdraw(&node, e, 90_000).await, "withdrawal_approved").len(), 1);
    wait(&node, PAST_THE_MOVE_WINDOW_MS).await;
    price(&node, BTC, "100000").await;
    accepted(order(&node, e, BTC, Side::Buy, "9", market("100000"), levels(None, Some("99600"))).await);
    let closes_out = closeout_price(&read(&node, e).await);
    assert!(closes_out < fixed("99600") && closes_out > fixed("99000"), "{closes_out}");
    wait(&node, 1_000).await;
    let events = price(&node, BTC, "99000").await;
    let fill = fill_of(&events, e);
    assert!(!fill.liquidation && fill.position.is_zero(), "{events:?}");
    assert!(events.contains(&triggered(e, BTC, LevelKind::StopLoss, "99600", fill.order_id)), "{events:?}");
    assert!(!events.iter().any(|event| matches!(event, Event::CloseOut { .. })), "{events:?}");

    // 8. F reverses a short that holds levels with a buy of twice its size, and its levels go naming the
    // reversal. G, holding $10,000, opens 9 BTC with levels beyond its close-out on both sides; at 99,000
    // it is closed out for margin, and its levels go naming the close-out.
    wait(&node, PAST_THE_MOVE_WINDOW_MS).await;
    price(&node, BTC, "100000").await;
    let named_f = levels(Some("98000"), Some("102000"));
    accepted(order(&node, f, BTC, Side::Sell, "1", market("100000"), named_f).await);
    let events = accepted(order(&node, f, BTC, Side::Buy, "2", market("100000"), levels(None, None)).await);
    let reversed = json!({ "type": "position_levels_removed", "account": id(f), "market": BTC,
        "take_profit": "98000", "stop_loss": "102000", "reason": "reversed" });
    assert_eq!(of_type(&events, "position_levels_removed"), [&reversed], "{events}");
    let read_f = read(&node, f).await;
    assert!(read_f["positions"]["1"]["qty"] == "1" && read_f.get("levels").is_none(), "{read_f}");
    assert_eq!(of_type(&withdraw(&node, g, 90_000).await, "withdrawal_approved").len(), 1);
    let named_g = levels(Some("105000"), Some("95000"));
    accepted(order(&node, g, BTC, Side::Buy, "9", market("100000"), named_g).await);
    assert!(closeout_price(&read(&node, g).await) > fixed("99000"));
    wait(&node, 1_000).await;
    let events = price(&node, BTC, "99000").await;
    assert!(events.iter().any(|event| matches!(event, Event::CloseOut { account, .. } if *account == id(g))));
    assert!(fill_of(&events, g).liquidation && !fired(&events), "{events:?}");
    assert!(events.contains(&removed(g, BTC, named_g, LevelsRemoved::CloseOut)), "{events:?}");

    // 9. H, at tier 0 with its limit of 25 working orders, holds a BTC and an ETH long and rests 24 BTC buy
    // limits. Levels on its BTC position take the 25th slot; levels on its ETH position are refused naming
    // tier 0; moving the BTC stop loss uses the slot the position holds.
    wait(&node, 1_000).await;
    price(&node, BTC, "99000").await;
    price(&node, ETH, "4000").await;
    accepted(order(&node, h, BTC, Side::Buy, "0.1", market("99000"), levels(None, None)).await);
    accepted(order(&node, h, ETH, Side::Buy, "1", market("4000"), levels(None, None)).await);
    let limit = OrderKind::Limit { price: Fixed::from_int(50_000) };
    for placed in 1..=24 {
        let events = accepted(order(&node, h, BTC, Side::Buy, "0.01", limit, levels(None, None)).await);
        assert_eq!(of_type(&events, "order_accepted").len(), 1, "{placed}: {events}");
    }
    let read_h = read(&node, h).await;
    assert_eq!((&read_h["volume"]["tier"], &read_h["volume"]["max_resting_orders"]), (&json!(0), &json!(25)));
    let events = accepted(set_levels(&node, h, h, BTC, levels(Some("120000"), Some("80000"))).await);
    assert_eq!(of_type(&events, "position_levels_set").len(), 1, "{events}");
    let events = accepted(set_levels(&node, h, h, ETH, levels(Some("5000"), Some("3000"))).await);
    let reason = json!({ "kind": "tier_order_limit", "tier": 0, "max": 25 });
    assert_eq!((&events[0]["type"], &events[0]["reason"]), (&json!("order_rejected"), &reason), "{events}");
    let events = accepted(set_levels(&node, h, h, BTC, levels(Some("120000"), Some("85000"))).await);
    assert_eq!(of_type(&events, "position_levels_set").len(), 1, "{events}");
    let held_h = json!({ "1": { "take_profit": "120000", "stop_loss": "85000" } });
    let read_h = read(&node, h).await;
    assert!(read_h["orders"].as_object().unwrap().len() == 24 && read_h["levels"] == held_h, "{read_h}");

    // 10. W holds 1,000 WTI with a stop loss at 68 over the weekend closure. A price of 67 on Saturday, with
    // the market shut, fires nothing; the first price after Sunday's reopening, 67, fills it at its closing
    // price there, 66.9866.
    price(&node, WTI, "70").await;
    accepted(order(&node, w, WTI, Side::Buy, "1000", market("70"), levels(None, Some("68"))).await);
    wait_until(&node, CLOSE - 1_000).await;
    price(&node, WTI, "70").await;
    wait_until(&node, CLOSE + 20 * 3_600_000).await;
    let events = price(&node, WTI, "67").await;
    assert!(!events.iter().any(|event| matches!(event, Event::Fill(_))) && !fired(&events), "{events:?}");
    assert_eq!(read(&node, w).await["levels"], json!({ "32": { "stop_loss": "68" } }));
    wait_until(&node, REOPEN + 60_000).await;
    let events = price(&node, WTI, "67").await;
    let fill = fill_of(&events, w);
    assert_eq!((fill.price, fill.position), (fixed("66.9866"), Fixed::ZERO), "{events:?}");
    assert!(events.contains(&triggered(w, WTI, LevelKind::StopLoss, "68", fill.order_id)), "{events:?}");
    assert!(events.contains(&removed(w, WTI, levels(None, Some("68")), LevelsRemoved::StopLoss)), "{events:?}");

    // 11. The history serves each kind of A's level events, and G's and F's removals name why.
    serve_history(&mut node).await;
    let fields = |events: Vec<Value>, keys: &[&str]| -> Vec<Vec<Value>> {
        events.iter().map(|event| keys.iter().map(|key| event[*key].clone()).collect()).collect()
    };
    let set = fields(history(&node, a, "position_levels_set").await, &["take_profit", "stop_loss"]);
    assert_eq!(set, [[json!("102000"), json!("98000")]]);
    let fired_a = fields(history(&node, a, "position_level_triggered").await, &["level", "price"]);
    assert_eq!(fired_a, [[json!("stop_loss"), json!("98000")]]);
    for (client, reason) in [(a, "stop_loss"), (f, "reversed"), (g, "close_out")] {
        let gone = fields(history(&node, client, "position_levels_removed").await, &["reason"]);
        assert_eq!(gone, [[json!(reason)]]);
    }
    // The public log replays against the enclave's key to the same accounts, levels included, and events;
    // a restarted node recovers the root it replays to and reports the same levels.
    let ids: Vec<AccountId> = wallets.iter().map(id).collect();
    let domain = crate::log::CheckpointDomain { chain_id: 31_337, vault: VAULT };
    let signer = AccountId(EnclaveKey::test_key().address());
    let mut replayed = replay_with_signer(&node.entries(), domain, signer).unwrap().engine;
    let accounts = ids.clone();
    let live =
        node.state.query(move |engine| accounts.iter().map(|id| engine.accounts()[id].clone()).collect::<Vec<_>>());
    let live = live.await;
    let replayed_accounts: Vec<_> = ids.iter().map(|id| replayed.accounts()[id].clone()).collect();
    assert_eq!(live.unwrap(), replayed_accounts);
    assert_eq!(
        (read(&node, d).await["levels"].clone(), read(&node, h).await["levels"].clone()),
        (held_d.clone(), held_h.clone())
    );
    let root = restart(&mut node, &genesis).await;
    assert_eq!(root, replayed.state_root());
    assert_eq!((read(&node, d).await["levels"].clone(), read(&node, h).await["levels"].clone()), (held_d, held_h));
}

/// A node whose tier table is dev's with the production genesis's resting-order limits, and whose
/// request buckets, which refill once an hour, hold more than the scenario sends; and its genesis.
async fn levels_node() -> (Node, Genesis) {
    let mut genesis = dev_config().genesis;
    for (tier, orders) in genesis.params.volume_tiers.iter_mut().zip(ACTIVATION_MAX_RESTING_ORDERS) {
        tier.max_resting_orders = orders;
    }
    let mut node = Node::start_from("levels-dev-scenario", true, true, false, &genesis, ProtocolVersion::LATEST).await;
    let burst = NonZeroU32::new(10_000).unwrap();
    node.state.limits = Arc::new(ApiLimits::with_quota(
        &dev_config().api,
        move |_| Quota::with_period(Duration::from_secs(3600)).unwrap().allow_burst(burst),
        DefaultClock::default(),
    ));
    node.app = router(node.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
    (node, genesis)
}

/// Serves `node`'s account history from an index built from its log, which lets reads in once it has
/// taken the command the log ends with now.
async fn serve_history(node: &mut Node) {
    let tip = node.state.query(Engine::last_seq).await.unwrap();
    let cfg = HistorySection { dir: scratch_dir("levels-dev-scenario-history"), retention: RETENTION, marks: None };
    let mut index = open_history(&cfg, &node.dir).unwrap();
    index.catch_up_to(tip);
    let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
    let live = node.state.stream.subscribe();
    let feed = Feed::new(live, node.dir.clone(), node.state.manifest.clone(), backfills, Some(index.resume_at()));
    let derived = node.state.history.derived().clone();
    let deriver = Deriver::new(derived.clone(), Some((node.dir.clone(), node.state.manifest.clone())));
    node.state.history = index.reader().with_derived(derived, None);
    node.tasks.push(tokio::spawn(async move { index.run(feed, deriver).await.expect("the index follows the log") }));
    node.app = router(node.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
}

/// `client`'s events of `kind` from the history route, newest first, once the index answers.
async fn history(node: &Node, client: &SigningKey, kind: &str) -> Vec<Value> {
    let path = format!("/v1/accounts/{}/history?kind={kind}", id(client));
    let answered = async {
        loop {
            let response = node.app.clone().oneshot(Request::get(&path).body(Body::empty()).unwrap()).await.unwrap();
            if response.status() == StatusCode::OK {
                let page: Value =
                    serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
                return page["events"].as_array().unwrap().clone();
            }
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "only catching up holds a read back");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    timeout(LOADED_WAIT, answered).await.expect("the index takes the log's last command")
}

/// Stops `node`'s sequencer and serves its API from one recovered from its log; the recovered root.
async fn restart(node: &mut Node, genesis: &Genesis) -> [u8; 32] {
    let policy = node.state.query(|engine| engine.access_policy().cloned()).await.unwrap();
    let sequencer = node.tasks.remove(1);
    sequencer.abort();
    let _ = sequencer.await;
    let (operator, clock, dir) = (node.state.operator, node.clock.clone(), node.dir.clone());
    let (published, manifest) = watch::channel(Manifest::default());
    let cfg = crate::sequencer::SequencerConfig {
        access_policy: policy,
        chain_id: 31_337,
        vault: VAULT,
        operator,
        clock,
        genesis: crate::sequencer::genesis_commands(genesis),
        ..sequencer_config(&dir, published)
    };
    let enclave = Arc::new(EnclaveKey::test_key());
    let mut recovered =
        Sequencer::recover(cfg, enclave, node.state.stream.clone(), mpsc::unbounded_channel().0).unwrap();
    let root = recovered.state_root();
    let (tx, rx) = mpsc::channel(64);
    node.tasks.push(tokio::spawn(async move { recovered.run(rx).await.unwrap() }));
    (node.state.tx, node.state.manifest) = (tx, manifest);
    node.app = router(node.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
    root
}

/// `client`'s signed Cancel all, with its next nonce and an expiry a minute from the node's time: its
/// status and events.
async fn cancel_all(node: &Node, client: &SigningKey) -> (StatusCode, Value) {
    let account = id(client);
    let nonce = node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap();
    let expires_at = node.clock.now_ms() + 60_000;
    let hash = eip712::cancel_all_hash(&eip712::domain(31_337, VAULT), account, nonce, expires_at);
    let auth = RequestAuth { nonce, expires_at, signature: eip712::sign(client, &hash) };
    node.post("/v1/orders/cancel-all", json!(CancelAllRequest { account, auth: Some(auth) })).await
}

/// The events of a request the route answered with 200.
fn accepted((status, events): (StatusCode, Value)) -> Value {
    assert_eq!(status, StatusCode::OK, "{events}");
    events
}

/// `client`'s account read.
async fn read(node: &Node, client: &SigningKey) -> Value {
    node.get(&format!("/v1/accounts/{}", id(client))).await
}

/// `market`'s entry in the market read.
async fn listed(node: &Node, market: MarketId) -> Value {
    let markets = node.get("/v1/markets").await;
    markets.as_array().unwrap().iter().find(|listed| listed["id"] == market).unwrap().clone()
}

/// Where the account read says its first position's market closes the account out.
fn closeout_price(read: &Value) -> Fixed {
    assert_eq!(read["closeouts"][0]["reach"], "at", "{read}");
    fixed(read["closeouts"][0]["price"].as_str().unwrap())
}

/// `client`'s one fill in `events`.
fn fill_of(events: &[Event], client: &SigningKey) -> Fill {
    let mut fills = events.iter().filter_map(|event| match event {
        Event::Fill(fill) if fill.account == id(client) => Some(fill.clone()),
        _ => None,
    });
    let fill = fills.next().unwrap_or_else(|| panic!("no fill: {events:?}"));
    assert!(fills.next().is_none(), "two fills: {events:?}");
    fill
}

/// Whether `events` fire any level.
fn fired(events: &[Event]) -> bool {
    events.iter().any(|event| matches!(event, Event::PositionLevelTriggered { .. }))
}

fn triggered(client: &SigningKey, market: MarketId, level: LevelKind, at: &str, order_id: u64) -> Event {
    Event::PositionLevelTriggered { account: id(client), market, level, price: fixed(at), order_id }
}

fn removed(client: &SigningKey, market: MarketId, levels: PositionLevels, reason: LevelsRemoved) -> Event {
    let PositionLevels { take_profit, stop_loss } = levels;
    Event::PositionLevelsRemoved { account: id(client), market, take_profit, stop_loss, reason }
}

fn set_event(client: &SigningKey, market: MarketId, levels: PositionLevels) -> Event {
    let PositionLevels { take_profit, stop_loss } = levels;
    Event::PositionLevelsSet { account: id(client), market, take_profit, stop_loss }
}

fn id(client: &SigningKey) -> AccountId {
    eip712::address(client.verifying_key())
}

fn fixed(value: &str) -> Fixed {
    value.parse().unwrap()
}
