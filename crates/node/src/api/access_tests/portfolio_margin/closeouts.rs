//! The `V81` dev scenario (#639): close-outs read the portfolio requirement, through the signed order
//! routes, the signed admin route over TLS and the account read on deterministic time. The node's
//! genesis names a crypto group (BTC-USD and ETH-USD) and a metals group (XAU-USD with XAG-USD, listed
//! for the scenario, on one weekly calendar with no weekend margin) in its parameters.

use chrono::NaiveDate;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, CloseAllRequest, CloseEntry};
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::portfolio::{GroupMember, OffsetGroup, PortfolioGrid};
use engine_core::session::Schedule;
use engine_core::types::MarketId;

use super::super::earned_leverage::withdraw;
use super::super::spreads::{Operator, fill_event, price};
use super::super::weekend_margin::CLOSE;
use super::super::*;
use crate::clock::Clock;

const BTC: MarketId = 1;
const ETH: MarketId = 2;
const XAU: MarketId = 30;
const XAG: MarketId = 31;
const AAPL: MarketId = 40;

fn member(market: MarketId, loading: &str, residual: i64) -> GroupMember {
    GroupMember {
        market,
        loading: loading.parse().unwrap(),
        residual_shock_bps: residual,
        max_residual_shock_bps: residual * 4,
    }
}

/// The grid the scenario launches with: BTC-USD with ETH-USD (loading 1.25) at 0.2% each, and XAU-USD
/// with XAG-USD at 0.2% each, every floor share 50%.
fn grid() -> PortfolioGrid {
    let group = |name: &str, members| OffsetGroup {
        name: name.into(),
        floor_share_bps: 5_000,
        max_floor_share_bps: 10_000,
        members,
    };
    PortfolioGrid {
        groups: vec![
            group("crypto", vec![member(BTC, "1", 20), member(ETH, "1.25", 20)]),
            group("metals", vec![member(XAU, "1", 20), member(XAG, "1", 20)]),
        ],
    }
}

/// The dev genesis with the grid in its parameters and XAG-USD listed as XAU-USD's twin.
fn genesis() -> Genesis {
    let mut genesis = dev_config().genesis;
    genesis.params.portfolio_grid = grid();
    let mut silver = genesis.markets.iter().find(|market| market.id == XAU).cloned().unwrap();
    (silver.id, silver.symbol) = (XAG, "XAG-USD".into());
    genesis.markets.push(silver);
    genesis.close_spread_ceilings.push(CloseSpreadCeiling { market: XAG, max_close_spread_bps: 25 });
    genesis
}

async fn read(node: &Node, account: AccountId) -> Value {
    node.get(&format!("/v1/accounts/{account}")).await
}

fn amount(read: &Value, key: &str) -> Fixed {
    read[key].as_str().unwrap().parse().unwrap()
}

/// The fills `events` carry, as `(market, close-out)`.
fn fills(events: &[Event]) -> Vec<(MarketId, bool)> {
    events.iter().filter_map(|e| if let Event::Fill(f) = e { Some((f.market, f.liquidation)) } else { None }).collect()
}

async fn wait(node: &Node, ms: u64) {
    node.move_time(ms).await;
    node.source.lock().unwrap().observed_at = node.clock.now_ms();
}

async fn wait_until(node: &Node, at: i64) {
    wait(node, u64::try_from(at - node.clock.now_ms()).expect("time moves forward")).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_close_outs_read_the_portfolio_requirement() {
    let genesis = genesis();
    let mut node =
        Node::start_from("portfolio-closeouts-dev-scenario", true, true, false, &genesis, ProtocolVersion::LATEST)
            .await;
    let operator = Operator::serving(&node).await;
    let (a, b, c, d, e) = (key(0xa1), key(0xa2), key(0xa3), key(0xa4), key(0xa5));
    let [a_id, b_id, c_id, d_id, e_id] = [&a, &b, &c, &d, &e].map(|client| eip712::address(client.verifying_key()));
    for client in [&a, &b, &c, &d, &e] {
        node.onboard(client).await;
    }
    // A, C, D and E keep $40,000, $25,000, $24,000 and $22,000 of their $100,000.
    for (client, out) in [(&a, 60_000), (&c, 75_000), (&d, 76_000), (&e, 78_000)] {
        withdraw(&node, client, out).await;
    }

    // 1. A opens $1M long BTC-USD and $1M short ETH-USD in two slices each, and $100k of AAPL, which is
    // in no group; B opens the same BTC-USD leg alone.
    price(&node, ETH, "4000").await;
    price(&node, BTC, "100000").await;
    price(&node, AAPL, "200").await;
    for _ in 0..2 {
        fill_event(&node, &a, BTC, Side::Buy, "5", "100000").await;
        fill_event(&node, &a, ETH, Side::Sell, "125", "4000").await;
    }
    fill_event(&node, &a, AAPL, Side::Buy, "500", "200").await;
    fill_event(&node, &b, BTC, Side::Buy, "10", "100000").await;
    let read_a = read(&node, a_id).await;
    assert_eq!(read_a["margin"], "30000", "{read_a}");
    assert_eq!(read_a["portfolio_margin"]["margin"], "26400", "{read_a}");
    assert_eq!(read_a["maintenance_margin"], "13200", "{read_a}");
    let read_b = read(&node, b_id).await;
    assert_eq!(read_b["portfolio_margin"]["margin"], read_b["margin"], "{read_b}");

    // 2. ETH-USD rises to 4,097 against BTC-USD at 100,000: A's equity, $14,905, is below $15,121.25, the
    // close-out level of its $30,242.50 per-position requirement, which would close it out under the rule
    // before V81, and above $13,321.25, that of its $26,642.50 portfolio requirement. Nothing is closed.
    let events = price(&node, ETH, "4097").await;
    assert!(fills(&events).is_empty(), "{events:?}");
    let read_a = read(&node, a_id).await;
    assert_eq!((&read_a["equity"], &read_a["margin"]), (&json!("14905"), &json!("30242.5")), "{read_a}");
    assert_eq!(read_a["maintenance_margin"], "13321.25", "{read_a}");
    assert!(amount(&read_a, "equity") < amount(&read_a, "margin").mul_bps(5_000), "{read_a}");
    let eth = 4098;

    // 3. The basis widens a little further and A still holds; then AAPL falls 12%. Closing AAPL alone
    // cannot restore the level, nor can closing the pair, so AAPL, which releases the most per dollar
    // closed, closes first, then the pair, whole: BTC-USD before ETH-USD.
    let events = price(&node, ETH, "4098").await;
    assert!(fills(&events).is_empty(), "{events:?}");
    let events = price(&node, AAPL, "176").await;
    assert_eq!(fills(&events), [(AAPL, true), (BTC, true), (ETH, true)], "{events:?}");
    assert!(read(&node, a_id).await["positions"].as_object().unwrap().is_empty());
    assert_eq!(read(&node, b_id).await["positions"][BTC.to_string()]["qty"], "10");

    // 4. E opens $1M long XAU-USD against $1M short XAG-USD, a group of two markets on one weekly calendar
    // with no weekend margin.
    price(&node, XAU, "2000").await;
    price(&node, XAG, "25").await;
    fill_event(&node, &e, XAU, Side::Buy, "500", "2000").await;
    fill_event(&node, &e, XAG, Side::Sell, "40000", "25").await;

    // 5. C opens a fresh BTC-USD and ETH-USD pair on $25,000, and BTC-USD falls to 98,660, where C is
    // below the close-out level of its per-position requirement and above that of its portfolio
    // requirement: C holds. Then ETH-USD's quotes go stale and BTC-USD falls to 98,400, below C's level:
    // ETH-USD waits at its last mark and BTC-USD closes alone.
    price(&node, ETH, &eth.to_string()).await;
    price(&node, BTC, "100000").await;
    fill_event(&node, &c, BTC, Side::Buy, "10", "100000").await;
    fill_event(&node, &c, ETH, Side::Sell, "250", &eth.to_string()).await;
    let events = price(&node, BTC, "98660").await;
    assert!(fills(&events).is_empty(), "{events:?}");
    let read_c = read(&node, c_id).await;
    let (equity, maintenance) = (amount(&read_c, "equity"), amount(&read_c, "maintenance_margin"));
    assert!(maintenance < equity && equity < amount(&read_c, "margin").mul_bps(5_000), "{read_c}");
    wait(&node, 2_000).await;
    let events = price(&node, BTC, "98400").await;
    assert_eq!(fills(&events), [(BTC, true)], "{events:?}");
    let read_c = read(&node, c_id).await;
    assert_eq!(read_c["positions"][ETH.to_string()]["qty"], "-250", "{read_c}");
    assert!(read_c["positions"].get(BTC.to_string()).is_none(), "{read_c}");
    let btc = 98_400;

    // 6. D opens a fresh pair the other way round, short BTC-USD and long ETH-USD, on $24,000. BTC-USD falls 1%, ETH-USD 2%, so that closing BTC-USD in the
    // first 10 s pays its whole gain as the profit fee and leaves D below the level of ETH-USD's bands
    // alone, though above that of the pair's portfolio requirement. A close-all of both fills both
    // entries before any check: neither is a close-out.
    price(&node, ETH, &eth.to_string()).await;
    price(&node, BTC, &btc.to_string()).await;
    fill_event(&node, &d, BTC, Side::Sell, "10", &btc.to_string()).await;
    fill_event(&node, &d, ETH, Side::Buy, "250", &eth.to_string()).await;
    let btc_down = btc - btc / 100;
    let eth_down = eth - eth * 2 / 100;
    price(&node, BTC, &btc_down.to_string()).await;
    let events = price(&node, ETH, &eth_down.to_string()).await;
    assert!(fills(&events).is_empty(), "{events:?}");
    let read_d = read(&node, d_id).await;
    assert!(amount(&read_d, "equity") > amount(&read_d, "maintenance_margin"), "{read_d}");
    let nonce = node.state.query(move |engine| engine.next_request_nonce(d_id)).await.unwrap();
    let quotes = [(BTC, btc_down.to_string()), (ETH, eth_down.to_string())];
    let events = close_all(&node, &d, nonce, &[(BTC, &quotes[0].1), (ETH, &quotes[1].1)]).await;
    assert_eq!(fills(&events), [(BTC, false), (ETH, false)], "{events:?}");
    assert!(!events.iter().any(|event| matches!(event, Event::CloseOut { .. })), "{events:?}");
    let btc_fill = events.iter().find_map(|e| if let Event::Fill(f) = e { Some(f) } else { None }).unwrap();
    assert_eq!(btc_fill.profit_fee, btc_fill.realised_pnl, "{btc_fill:?}");
    assert!(read(&node, d_id).await["positions"].as_object().unwrap().is_empty());

    // 4 (continued). E holds the pair to Friday's close, XAG-USD up 0.7% on the week: E is above the
    // close-out level of its portfolio requirement and below its per-position requirement in full. With no
    // weekend margin the metals keep no weekend gap, so nothing more is asked of E before or at the close,
    // and E holds the pair over the weekend.
    wait_until(&node, CLOSE - 1_000).await;
    price(&node, XAU, "2000").await;
    price(&node, XAG, "25.175").await;
    let read_e = read(&node, e_id).await;
    let equity = amount(&read_e, "equity");
    assert!(amount(&read_e, "maintenance_margin") < equity, "{read_e}");
    assert!(equity < amount(&read_e, "margin"), "{read_e}");
    assert!(read_e.get("weekend_margins").is_none(), "{read_e}");
    node.clock.advance(Duration::from_millis(2_000));
    node.retrust().await;
    let events = node.state.submit(Command::Tick).await.unwrap();
    assert!(fills(&events).is_empty() && !events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    let read_e = read(&node, e_id).await;
    assert_eq!(read_e["positions"][XAU.to_string()]["qty"], "500", "{read_e}");
    assert_eq!(read_e["positions"][XAG.to_string()]["qty"], "-40000", "{read_e}");

    // 7. The operator announces a higher residual shock for BTC-USD and ETH-USD, in force seven days on:
    // a set-params before its time is refused and one after applies. A lower shock, one past its bound
    // and a changed loading are refused at the announcement.
    let launch = genesis.params.portfolio_grid.clone();
    let mut raised = launch.clone();
    for member in &mut raised.groups[0].members {
        member.residual_shock_bps = 30;
    }
    let notice = |grid: PortfolioGrid, at: i64| {
        AdminCommand::AnnounceParams(ParamsNotice {
            profit_fee_bps: None,
            closeout_level_bps: None,
            stress: None,
            profit_fee_curve: None,
            volume_tiers: None,
            rollover: None,
            corporate_bounds: None,
            portfolio_grid: Some(grid),
            effective_at: at,
            score_tiers: None,
        })
    };
    let due = node.clock.now_ms() + NOTICE_MS + 60_000;
    operator.applies(&node, notice(raised.clone(), due)).await;
    let house = node.get("/v1/house/state").await;
    assert_eq!(house["params_notice"]["portfolio_grid"], serde_json::to_value(&raised).unwrap(), "{house}");
    let mut params = node.state.query(|engine| engine.params().cloned()).await.unwrap().unwrap();
    params.portfolio_grid = raised.clone();
    operator.refuses(&node, AdminCommand::SetParams(params.clone()), &format!("takes effect from {due}")).await;
    wait_until(&node, due).await;
    operator.applies(&node, AdminCommand::SetParams(params)).await;
    let house = node.get("/v1/house/state").await;
    assert_eq!(house["params"]["portfolio_grid"], serde_json::to_value(&raised).unwrap(), "{house}");
    let later = node.clock.now_ms() + NOTICE_MS + 60_000;
    let mut lower = launch.clone();
    lower.groups[0].members[0].residual_shock_bps = 10;
    operator.refuses(&node, notice(lower, later), "must be from its launch value 20 to its bound 80").await;
    let mut beyond = launch.clone();
    beyond.groups[0].members[0].residual_shock_bps = 90;
    operator
        .refuses(&node, notice(beyond, later), "market 1's bound on its residual shock must be from the shock")
        .await;
    let mut loading = launch.clone();
    loading.groups[0].members[1].loading = "1.5".parse().unwrap();
    operator.refuses(&node, notice(loading, later), "fixed at its launch grid").await;

    // 8. The operator relists both metals with a holiday added, in one command, which applies to each; one
    // member relisted alone with another holiday is refused.
    let specs = node.state.query(|engine| [XAU, XAG].map(|id| engine.markets()[&id].spec.clone())).await.unwrap();
    let holiday = |date: (i32, u32, u32), spec: &engine_core::market::MarketSpec| {
        let mut spec = spec.clone();
        let Schedule::Weekly { closed_dates, .. } = &mut spec.schedule else { unreachable!("weekly") };
        closed_dates.push(NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap());
        spec
    };
    let both = specs.iter().map(|spec| holiday((2027, 3, 17), spec)).collect::<Vec<_>>();
    operator.applies(&node, AdminCommand::RelistMarkets { specs: both.clone() }).await;
    let held =
        node.state.query(|engine| [XAU, XAG].map(|id| engine.markets()[&id].spec.schedule.clone())).await.unwrap();
    assert_eq!(held.to_vec(), both.iter().map(|spec| spec.schedule.clone()).collect::<Vec<_>>());
    let alone = holiday((2027, 3, 18), &both[0]);
    operator
        .refuses(&node, AdminCommand::RelistMarkets { specs: vec![alone] }, "differ in asset class or schedule")
        .await;

    // 9. The public log replays to the same state, and a restart recovers the root it replays to.
    let domain = crate::log::CheckpointDomain { chain_id: 31_337, vault: VAULT };
    let signer = AccountId(EnclaveKey::test_key().address());
    let mut replayed = replay_with_signer(&node.entries(), domain, signer).unwrap().engine;
    let ids = [a_id, b_id, c_id, d_id, e_id];
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
