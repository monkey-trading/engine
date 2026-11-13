//! The `V55` dev scenario: a realised gain pays the profit fee its hold gives, the hold runs from the
//! position's open time, which an add moves and a flip restarts, a close-out pays at the hold of what
//! it closes, and the operator moves the fee only up and only after notice, through the signed order
//! and admin routes, the admin route over TLS, and the account and house reads, on deterministic time;
//! the public log replays to the same state, and a restart recovers it.

use engine_core::command::AdminCommand;
use engine_core::event::Fill;
use engine_core::fixed::Rounding;
use engine_core::holding_fee::DEFAULT_PROFIT_FEE_CURVE;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::types::MarketId;

use super::spreads::{Operator, fill_event, fund, price};
use super::*;
use crate::clock::Clock;

const BTC: MarketId = 1;
const ETH: MarketId = 2;

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_a_gain_pays_the_fee_its_hold_gives_and_the_fee_moves_only_up() {
    let mut node = Node::start("holding-fee-dev-scenario", true).await;
    let operator = Operator::serving(&node).await;
    let (a, b, c, d) = (key(0x71), key(0x72), key(0x73), key(0x74));
    for client in [&a, &b, &c, &d] {
        node.onboard(client).await;
    }
    for client in [&a, &b, &d] {
        fund(&node, client, 1_000_000).await;
    }

    // 1. A opens a $100k BTC long, and the index rises 1%.
    price(&node, BTC, "100000").await;
    let opened = node.clock.now_ms();
    fill_event(&node, &a, BTC, Side::Buy, "1", "100000").await;
    // 2. A closes a tenth 5 s on, a quarter at 60 s, adds its first size at 10 minutes, closes half at
    // 20 minutes and the rest at 45: each close pays the curve's rate at the hold its clock gives.
    let closes = [(5_000, "0.1", 5_000, 10_000), (55_000, "0.25", 60_000, 6_421)];
    for (after, qty, hold, rate) in closes {
        wait(&node, after).await;
        price(&node, BTC, "101000").await;
        pays(&fill_event(&node, &a, BTC, Side::Sell, qty, "101000").await, hold, rate);
    }
    wait(&node, 540_000).await;
    price(&node, BTC, "101000").await;
    let add = fill_event(&node, &a, BTC, Side::Buy, "1", "101000").await;
    assert_eq!((add["hold_ms"].as_i64(), add["profit_fee_bps"].as_i64()), (None, None), "{add}");
    // 0.65 held from the open and 1 added at 600 s: the open time moves 600 s x 1 / 1.65, rounded up.
    let moved = opened + 363_637;
    let account = node.get(&format!("/v1/accounts/{}", eip712::address(a.verifying_key()))).await;
    let close_fee = &account["close_fees"][BTC.to_string()];
    assert_eq!(close_fee["opened_at"].as_i64(), Some(moved), "{account}");
    // Held 236.363 s, a close pays 36.80% now, and 36.79% from 61 ms on.
    assert_eq!((close_fee["fee_bps"].as_i64(), close_fee["falls_at"].as_i64()), (Some(3_680), Some(moved + 236_424)));
    for (after, hold, rate) in [(600_000, 836_363, 1_176), (1_500_000, 2_336_363, 1_000)] {
        wait(&node, after).await;
        price(&node, BTC, "101000").await;
        pays(&fill_event(&node, &a, BTC, Side::Sell, "0.825", "101000").await, hold, rate);
    }

    // 3. B's short flips 30 s on: its gain pays 78.53%, and the long the rest opens starts now.
    fill_event(&node, &b, BTC, Side::Sell, "1", "101000").await;
    wait(&node, 30_000).await;
    price(&node, BTC, "100000").await;
    let flip = fill_event(&node, &b, BTC, Side::Buy, "2", "100000").await;
    pays(&flip, 30_000, 7_853);
    let account = node.get(&format!("/v1/accounts/{}", eip712::address(b.verifying_key()))).await;
    let now = node.clock.now_ms();
    assert_eq!(account["close_fees"][BTC.to_string()]["opened_at"].as_i64(), Some(now), "{account}");

    // 4. C holds a BTC long that gains and an ETH long that loses; ETH's fall closes C out of both, the
    // BTC gain paying the whole of itself at its 5 s hold.
    price(&node, ETH, "2000").await;
    fill_event(&node, &c, BTC, Side::Buy, "23", "100000").await;
    fill_event(&node, &c, ETH, Side::Buy, "750", "2000").await;
    wait(&node, 5_000).await;
    price(&node, BTC, "100200").await;
    let events = price(&node, ETH, "1880").await;
    let closed: Vec<&Fill> =
        events.iter().filter_map(|event| if let Event::Fill(fill) = event { Some(fill) } else { None }).collect();
    let [eth, btc] = closed[..] else { panic!("two close-out fills: {events:?}") };
    assert!(eth.liquidation && btc.liquidation && eth.market == ETH && btc.market == BTC);
    assert!(eth.realised_pnl.is_negative() && eth.profit_fee.is_zero(), "{eth:?}");
    assert!(btc.realised_pnl.is_positive(), "{btc:?}");
    assert_eq!((btc.hold_ms, btc.profit_fee_bps, btc.profit_fee), (Some(5_000), Some(10_000), btc.realised_pnl));

    // 5. A cut to 5% is refused; a raise to 15% applies from its notice's time, and then a close at 10
    // minutes pays the curve's 18.28% and one at 30 minutes the raised base.
    let due = node.clock.now_ms() + NOTICE_MS;
    let notice = |profit_fee_bps| {
        AdminCommand::AnnounceParams(ParamsNotice {
            profit_fee_bps: Some(profit_fee_bps),
            closeout_level_bps: None,
            stress: None,
            profit_fee_curve: None,
            effective_at: due,
            score_tiers: None,
        })
    };
    operator.refuses(&node, notice(500), "profit_fee_bps 500 is below its launch value 1000").await;
    operator.applies(&node, notice(1_500)).await;
    let house = node.get("/v1/house/state").await;
    assert_eq!(house["params_notice"]["profit_fee_bps"], 1_500, "{house}");
    let curve = serde_json::to_value(DEFAULT_PROFIT_FEE_CURVE).unwrap();
    assert_eq!(house["params"]["profit_fee_curve"], curve, "{house}");
    let mut raised = dev_config().genesis.params;
    raised.profit_fee_bps = 1_500;
    operator.refuses(&node, AdminCommand::SetParams(raised.clone()), &format!("takes effect from {due}")).await;
    wait(&node, NOTICE_MS as u64).await;
    operator.applies(&node, AdminCommand::SetParams(raised)).await;
    for (hold, rate) in [(600_000, 1_828), (1_800_000, 1_500)] {
        price(&node, BTC, "100000").await;
        fill_event(&node, &d, BTC, Side::Buy, "1", "100000").await;
        wait(&node, hold as u64).await;
        price(&node, BTC, "101000").await;
        pays(&fill_event(&node, &d, BTC, Side::Sell, "1", "101000").await, hold, rate);
    }

    // 6. The public log replays to the same state, and a restart recovers the root it replays to.
    let accounts = [&a, &b, &c, &d].map(|client| eip712::address(client.verifying_key()));
    let mut replayed = replay(&node.entries()).unwrap().engine;
    let live =
        node.state.query(move |engine| accounts.map(|account| engine.accounts()[&account].clone())).await.unwrap();
    assert_eq!(accounts.map(|account| replayed.accounts()[&account].clone()), live);
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

/// Moves the node's time forward by `ms`, with the screening source observing at the new time.
async fn wait(node: &Node, ms: u64) {
    node.move_time(ms).await;
    node.source.lock().unwrap().observed_at = node.clock.now_ms();
}

/// Checks a close's fill: the hold it names, the rate it pays, and a fee of that rate on its realised
/// gain after the spread, rounded up, or none on a loss.
fn pays(fill: &Value, hold_ms: i64, fee_bps: i64) {
    assert_eq!((fill["hold_ms"].as_i64(), fill["profit_fee_bps"].as_i64()), (Some(hold_ms), Some(fee_bps)), "{fill}");
    let amount = |key: &str| -> Fixed { fill[key].as_str().unwrap().parse().unwrap() };
    let realised = amount("realised_pnl");
    assert!(realised.is_positive(), "{fill}");
    assert_eq!(amount("profit_fee"), realised.mul_bps_rounded(fee_bps, Rounding::Up), "{fill}");
}
