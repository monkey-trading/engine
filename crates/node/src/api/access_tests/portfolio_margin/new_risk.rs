//! The `V84` dev scenario (#916): new risk and withdrawals read the portfolio requirement, through the
//! signed order, close-all and withdrawal routes, the order preview, the signed quote stream and the account
//! read, on deterministic time. The node's genesis names one crypto group, BTC-USD with ETH-USD (loading
//! 1.25), at a 0.05% residual shock and a 10% floor share, lists both at 2.5% bands, and takes the earned
//! leverage scenario's tier table: floors of 5% at tier 0 and 2% at tier 1, tier 2 at $400k of counted
//! volume. Each band of the initial requirement keeps its tier's floor, so a lone position keeps its tier's
//! leverage, and a hedged book's credit applies at every tier (#913, D8 as revised on 17 November).

use engine_core::command::{CloseAllRequest, CloseEntry};
use engine_core::market::Tier;
use engine_core::portfolio::{GroupMember, OffsetGroup, PortfolioGrid};
use engine_core::types::MarketId;
use engine_core::volume_tier::MIN_HOLD_MS;

use super::super::earned_leverage::{wait, withdraw};
use super::super::spreads::{fill_event, order_events, price, quote};
use super::super::*;
use crate::clock::Clock;

const BTC: MarketId = 1;
const ETH: MarketId = 2;

/// The scenario's genesis: the dev genesis with the group, the bands and the tier table above.
fn genesis() -> Genesis {
    let mut genesis = dev_config().genesis;
    let member = |market, loading: &str| GroupMember {
        market,
        loading: loading.parse().unwrap(),
        residual_shock_bps: 5,
        max_residual_shock_bps: 20,
    };
    let crypto = OffsetGroup {
        name: "crypto".into(),
        floor_share_bps: 1_000,
        max_floor_share_bps: 10_000,
        members: vec![member(BTC, "1"), member(ETH, "1.25")],
    };
    genesis.params.portfolio_grid = PortfolioGrid { groups: vec![crypto] };
    for market in genesis.markets.iter_mut().filter(|market| [BTC, ETH].contains(&market.id)) {
        market.margin_tiers = vec![Tier { up_to: None, bps: 250 }];
    }
    let tiers = [(0, 500), (100_000, 200), (400_000, 0), (10_000_000, 0)];
    for (tier, (threshold, floor)) in genesis.params.volume_tiers.iter_mut().zip(tiers) {
        (tier.threshold, tier.day_divisor, tier.margin_floor_bps) = (Fixed::from_int(threshold), 1, floor);
    }
    genesis
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_new_risk_and_withdrawals_read_the_portfolio_requirement() {
    let genesis = genesis();
    let mut node = Node::start_from("portfolio-new-risk-dev-scenario", true, true, false, &genesis).await;
    let (a, b, c) = (key(0xb1), key(0xb2), key(0xb3));
    let [a_id, b_id, c_id] = [&a, &b, &c].map(|client| eip712::address(client.verifying_key()));
    for client in [&a, &b, &c] {
        node.onboard(client).await;
    }
    price(&node, BTC, "100000").await;
    price(&node, ETH, "4000").await;

    // 0. A earns tier 2: $410k of BTC-USD held a minute counts, and it closes the position.
    fill_event(&node, &a, BTC, Side::Buy, "4.1", "100000").await;
    wait(&node, MIN_HOLD_MS as u64).await;
    prices(&node, "100000", "4000").await;
    fill_event(&node, &a, BTC, Side::Sell, "4.1", "100000").await;
    assert_eq!(read(&node, a_id).await["volume"]["tier"], 2);

    // 1. On about $20,000, A opens $1M of BTC-USD long against $1M of ETH-USD short in alternating slices,
    // each leaving the book hedged: per-position margin would ask $50,000, the portfolio requirement less.
    withdraw_to(&node, &a, a_id, 20_000).await;
    for _ in 0..2 {
        fill_event(&node, &a, BTC, Side::Buy, "5", "100000").await;
        fill_event(&node, &a, ETH, Side::Sell, "125", "4000").await;
    }
    let read_a = read(&node, a_id).await;
    assert_eq!(read_a["margin"], "50000", "{read_a}");
    let (equity, initial) = (amount(&read_a, "equity"), amount(&read_a, "initial_margin"));
    assert!(initial < equity && equity < Fixed::from_int(20_000), "{read_a}");
    assert_eq!(read_a["initial_margin"], read_a["portfolio_margin"]["initial_margin"], "{read_a}");

    // 2. B, at tier 0, holds its tier's 5% floor on a lone position: on about $20,000 a $401k BTC-USD long
    // is refused and $390k opens. Its hedged book passes 20x gross: $490k of BTC-USD against $400k of
    // ETH-USD opens on the same equity.
    withdraw_to(&node, &b, b_id, 20_000).await;
    let refused = rejection(&order_events(&node, &b, BTC, Side::Buy, "4.01", "100000").await);
    assert_eq!(refused["kind"], "insufficient_margin", "{refused}");
    assert_eq!(refused["required"], "20050", "{refused}");
    fill_event(&node, &b, BTC, Side::Buy, "3.9", "100000").await;
    fill_event(&node, &b, ETH, Side::Sell, "100", "4000").await;
    fill_event(&node, &b, BTC, Side::Buy, "1", "100000").await;
    let read_b = read(&node, b_id).await;
    assert_eq!((read_b["volume"]["tier"].as_u64(), &read_b["max_leverage"][BTC.to_string()]), (Some(0), &json!("20")));
    let gross = Fixed::from_int(890_000);
    assert!(gross > amount(&read_b, "equity").mul_int(20), "{read_b}");

    // 3. A withdraws up to its portfolio initial requirement, and is refused a dollar more.
    let free = amount(&read(&node, a_id).await, "withdrawable");
    let whole = i64::try_from(free.raw() / Fixed::ONE.raw()).unwrap();
    let events = withdraw(&node, &a, whole + 1).await;
    assert_eq!(events[0]["reason"]["kind"], "insufficient_funds", "{events}");
    let events = withdraw(&node, &a, whole).await;
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "withdrawal_queued"), "{events}");

    // 4. BTC-USD rises 0.5%. A previews closing its winning BTC-USD leg alone: ETH-USD's $25,000 of bands,
    // which its equity cannot meet. An order closing it is refused with the preview's reason, and the
    // signed quote's close says so, while its selling side quotes a partial close that fits; a close-all of
    // both legs fills both.
    prices(&node, "100500", "4000").await;
    let preview = preview_of(&node, a_id, BTC, "sell", "10", true).await;
    assert_eq!(preview["refused"]["kind"], "insufficient_margin", "{preview}");
    assert_eq!((&preview["initial_margin"], &preview["fill"]["qty"]), (&json!("25000"), &json!("10")), "{preview}");
    let events = order(&node, &a, BTC, Side::Sell, "10", market_order("100500"), true).await;
    assert_eq!(rejection(&events), preview["refused"], "{events}");
    let quoted = quote(&node, a_id, BTC).await;
    assert_eq!(quoted["close"]["declined"], preview["refused"], "{quoted}");
    // The selling side quotes each size on its own, the whole close included: a partial close that fits, and
    // each step as an order of its size is previewed.
    let bid = quoted["bid"].as_array().unwrap().clone();
    let fits = |step: &Value| step.get("declined").is_none() && amount(step, "qty") < Fixed::from_int(10);
    assert!(bid.iter().any(fits), "{quoted}");
    for step in &bid {
        let own = preview_of(&node, a_id, BTC, "sell", step["qty"].as_str().unwrap(), false).await;
        assert_eq!(step.get("declined").cloned().unwrap_or(Value::Null), own["refused"], "{step} against {own}");
    }
    let nonce = node.state.query(move |engine| engine.next_request_nonce(a_id)).await.unwrap();
    let events = close_all(&node, &a, nonce, &[(BTC, "100500"), (ETH, "4000")]).await;
    assert_eq!(fills(&events), [(BTC, false), (ETH, false)], "{events:?}");

    // 6. (Before 5 on the clock, so its fall in BTC-USD finds A with no positions.) C, at tier 0, holds $1M of BTC-USD
    // on about $52,000, and BTC-USD falls 1%: below its initial requirement, above its close-out level. Its ETH-USD
    // bid steps at 62.5, where a hedge first fills, and at 1,250, which over-hedges. Between them the previews refuse
    // a small hedge and admit a larger one, and refuse past the over-hedge, each as an order of its size is; each
    // step's verdict is its own size's preview.
    prices(&node, "100000", "4000").await;
    fill_event(&node, &c, BTC, Side::Buy, "10", "100000").await;
    withdraw_to(&node, &c, c_id, 52_000).await;
    prices(&node, "99000", "4000").await;
    let read_c = read(&node, c_id).await;
    let equity = amount(&read_c, "equity");
    assert!(amount(&read_c, "maintenance_margin") < equity && equity < amount(&read_c, "initial_margin"), "{read_c}");
    let bid = quote(&node, c_id, ETH).await["bid"].clone();
    for step in bid.as_array().unwrap().iter().take(3) {
        let own = preview_of(&node, c_id, ETH, "sell", step["qty"].as_str().unwrap(), false).await;
        assert_eq!(step.get("declined").cloned().unwrap_or(Value::Null), own["refused"], "{step} against {own}");
    }
    let verdicts = [("20", false), ("30", false), ("45", true), ("100", true), ("300", true), ("400", false)];
    let mut previews = Vec::new();
    for (qty, fills) in verdicts {
        let own = preview_of(&node, c_id, ETH, "sell", qty, false).await;
        assert_eq!(own["refused"].is_null(), fills, "{qty}: {own}");
        previews.push((qty, own));
    }
    for (qty, own) in previews.iter().filter(|(_, own)| !own["refused"].is_null()) {
        let events = order(&node, &c, ETH, Side::Sell, qty, market_order("4000"), false).await;
        assert_eq!(rejection(&events), own["refused"], "{qty}: {events}");
    }
    let (qty, own) = &previews[2];
    let events = order(&node, &c, ETH, Side::Sell, qty, market_order("4000"), false).await;
    let fill = events.as_array().unwrap().iter().find(|event| event["type"] == "fill").cloned();
    assert_eq!(fill.map(|fill| fill["price"].clone()), Some(own["fill"]["price"].clone()), "{events}");

    // 5. A reopens half the pair and withdraws down to its requirement again. ETH-USD's quotes go stale:
    // adding to BTC-USD is checked with the pair at per-position margin, $25,000 and more, and refused as
    // the preview says.
    prices(&node, "100000", "4000").await;
    fill_event(&node, &a, BTC, Side::Buy, "5", "100000").await;
    fill_event(&node, &a, ETH, Side::Sell, "125", "4000").await;
    let free = amount(&read(&node, a_id).await, "withdrawable");
    let events = withdraw(&node, &a, i64::try_from(free.raw() / Fixed::ONE.raw()).unwrap()).await;
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "withdrawal_queued"), "{events}");
    wait(&node, 2_000).await;
    price(&node, BTC, "100000").await;
    let preview = preview_of(&node, a_id, BTC, "buy", "0.1", false).await;
    assert_eq!(preview["refused"]["kind"], "insufficient_margin", "{preview}");
    assert!(amount(&preview, "initial_margin") > Fixed::from_int(25_000), "{preview}");
    let events = order(&node, &a, BTC, Side::Buy, "0.1", market_order("100000"), false).await;
    assert_eq!(rejection(&events), preview["refused"], "{events}");
    price(&node, ETH, "4000").await;

    // 7. A rests a stop on its whole BTC-USD leg, a close an order could not make: ETH-USD alone would need
    // $12,500. BTC-USD falls through it: the stop fills, and A, below its initial requirement but above its
    // close-out level, is not closed out.
    prices(&node, "100000", "4000").await;
    let preview = preview_of(&node, a_id, BTC, "sell", "5", true).await;
    assert_eq!(preview["refused"]["kind"], "insufficient_margin", "{preview}");
    let stop = OrderKind::Stop { price: "99900".parse().unwrap() };
    let events = order(&node, &a, BTC, Side::Sell, "5", stop, true).await;
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "order_accepted"), "{events}");
    wait(&node, 500).await;
    price(&node, ETH, "4000").await;
    let events = price(&node, BTC, "99850").await;
    assert_eq!(fills(&events), [(BTC, false)], "{events:?}");
    let read_a = read(&node, a_id).await;
    let equity = amount(&read_a, "equity");
    assert!(amount(&read_a, "maintenance_margin") < equity && equity < amount(&read_a, "initial_margin"), "{read_a}");
    assert_eq!(read_a["positions"][ETH.to_string()]["qty"], "-125", "{read_a}");

    // 8. The public log replays to the same state, and a restart recovers the root it replays to.
    let domain = crate::log::CheckpointDomain { chain_id: 31_337, vault: VAULT };
    let signer = AccountId(EnclaveKey::test_key().address());
    let mut replayed = replay_with_signer(&node.entries(), domain, signer).unwrap().engine;
    let ids = [a_id, b_id, c_id];
    let live = node.state.query(move |engine| ids.map(|id| engine.accounts()[&id].clone())).await.unwrap();
    assert_eq!(ids.map(|id| replayed.accounts()[&id].clone()), live);
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

// ── Helpers ──

async fn read(node: &Node, account: AccountId) -> Value {
    node.get(&format!("/v1/accounts/{account}")).await
}

fn amount(read: &Value, key: &str) -> Fixed {
    read[key].as_str().unwrap_or_else(|| panic!("{key}: {read}")).parse().unwrap()
}

/// Prices BTC-USD and ETH-USD at `btc` and `eth`.
async fn prices(node: &Node, btc: &str, eth: &str) {
    price(node, BTC, btc).await;
    price(node, ETH, eth).await;
}

/// Withdraws whole dollars from `client`, `account`, until its equity is about `equity`.
async fn withdraw_to(node: &Node, client: &SigningKey, account: AccountId, equity: i64) {
    let now = amount(&read(node, account).await, "equity");
    let out = i64::try_from(now.raw() / Fixed::ONE.raw()).unwrap() - equity;
    let events = withdraw(node, client, out).await;
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "withdrawal_queued"), "{events}");
}

/// `account`'s preview of a market order of `qty` on `side` in `market`, through the route.
async fn preview_of(
    node: &Node,
    account: AccountId,
    market: MarketId,
    side: &str,
    qty: &str,
    reduce_only: bool,
) -> Value {
    let path =
        format!("/v1/accounts/{account}/preview?market={market}&side={side}&qty={qty}&reduce_only={reduce_only}");
    let answer = node.get(&path).await;
    assert_eq!((&answer["account"], &answer["qty"]), (&json!(account), &json!(qty)), "{answer}");
    assert!(answer["seq"].is_u64(), "{answer}");
    answer
}

/// A signed order of `qty` on `side` in `market`, of `kind`, reduce-only when `reduce_only`; its events.
async fn order(
    node: &Node,
    client: &SigningKey,
    market: MarketId,
    side: Side,
    qty: &str,
    kind: OrderKind,
    reduce_only: bool,
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
        reduce_only,
        auth: None,
        take_profit: None,
        stop_loss: None,
    };
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(client, &hash) });
    let (status, events) = node.post("/v1/orders", json!(request)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    events
}

/// The rejection reason among `events`.
fn rejection(events: &Value) -> Value {
    let refused = events.as_array().unwrap().iter().find(|event| event["type"] == "order_rejected");
    refused.unwrap_or_else(|| panic!("{events}"))["reason"].clone()
}

/// The fills `events` carry, as `(market, close-out)`.
fn fills(events: &[Event]) -> Vec<(MarketId, bool)> {
    events.iter().filter_map(|e| if let Event::Fill(f) = e { Some((f.market, f.liquidation)) } else { None }).collect()
}

/// A signed close-all of `markets`, each quoted at its price in `quotes`, through the route.
async fn close_all(node: &Node, client: &SigningKey, nonce: u64, quotes: &[(MarketId, &str)]) -> Vec<Event> {
    let account = eip712::address(client.verifying_key());
    let closes = quotes.iter().map(|(market, quoted)| CloseEntry { market: *market, quoted: quoted.parse().unwrap() });
    let mut request = CloseAllRequest { account, closes: closes.collect(), max_slippage_bps: Some(10_000), auth: None };
    let expires_at = node.clock.now_ms() + 60_000;
    let hash = eip712::close_all_hash(&eip712::domain(31_337, VAULT), &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(client, &hash) });
    let (status, events) = node.post("/v1/positions/close-all", json!(request)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    serde_json::from_value(events).unwrap()
}

/// A market order quoted at `quoted`, at the house's slippage bound.
fn market_order(quoted: &str) -> OrderKind {
    OrderKind::Market { quoted: quoted.parse().unwrap(), max_slippage_bps: None }
}
