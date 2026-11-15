//! The operator channel's dev scenario via `engine admin`: a market listed once, then every market
//! reduce-only.

use super::*;
use engine_core::market::MarketSpec;

/// The feature's dev scenario: the operator lists a new market with `engine admin`, the same signed
/// request sent again is refused, and with every market reduce-only an open is refused while a
/// close fills.
#[tokio::test]
async fn dev_scenario_list_a_market_once_then_every_market_reduce_only_refuses_opens_and_fills_closes() {
    let node = Node::start("dev-scenario").await;
    let url = format!("https://{}", node.serve().await);
    let dir = scratch_dir("dev-scenario-files");
    fs::create_dir_all(&dir).unwrap();
    let sol = dir.join("sol.toml");
    let spec =
        toml::to_string(&MarketSpec { id: 3, symbol: "SOL-USD".into(), ..dev_config().genesis.markets[0].clone() });
    fs::write(&sol, spec.unwrap()).unwrap();
    let key = || KeyEnv { key_file: None, key: Some(HexKey(format!("0x{}", hex::encode(DEV_KEY)))) };

    let listing =
        admin::run_with_clock(node.args(&url, &["upsert-market", sol.to_str().unwrap()]), key(), || NOW).await.unwrap();
    let err = admin::submit(&node.api(&url), &listing).await.unwrap_err();
    assert!(err.to_string().contains("409 Conflict"), "{err:#}");
    let (_, markets) = node.get("/v1/markets").await;
    let listed = markets.as_array().unwrap().iter().filter(|m| m["symbol"] == "SOL-USD").count();
    assert_eq!(listed, 1, "{markets}");

    node.fund_and_buy_btc().await;
    admin::run_with_clock(node.args(&url, &["set-reduce-only", "true"]), key(), || NOW).await.unwrap();
    let (_, opened) = node.post("/v1/orders", btc_order("buy")).await;
    assert_eq!(opened[0]["reason"]["kind"], "reduce_only", "{opened}");
    let (_, closed) = node.post("/v1/orders", btc_order("sell")).await;
    let fill = closed.as_array().unwrap().iter().find(|e| e["type"] == "fill").expect("the close fills");
    assert_eq!(fill["position"], "0", "{closed}");

    // The log holds both admin commands with the operator's signature, and replays.
    let entries = node.entries();
    let logged = admin_entries(&entries);
    assert_eq!(logged.len(), 2);
    assert_eq!(logged[0], (listing, vec![Event::MarketOpened { market: 3 }, Event::AdminApplied]));
    let replayed = replay(&entries).unwrap().engine;
    assert!(replayed.markets().values().all(|market| replayed.is_reduce_only(market)));
}
