//! Local TLS operator scenario with controlled venue boundaries and public-log recovery.

use super::*;

use engine_core::command::OrderKind;
use engine_core::corporate::{Adjustment, CorporateAction};
use engine_core::types::AssetClass;

use crate::sequencer::Request;

const STOCK: u32 = 40;
const SHORT: &str = "0x00000000000000000000000000000000000000bb";

#[tokio::test]
async fn corporate_action_signatures_cover_the_required_rate_and_calendar_boundaries() {
    let node = Node::start("corporate-signature").await;
    let action = CorporateAction {
        id: "signed-input".into(),
        market: STOCK,
        previous_close: NOW + 60_000,
        ex_open: NOW + 120_000,
        adjustment: Adjustment::Dividend {
            gross_per_unit: Fixed::from_int(4),
            withholding_rate: "0.25".parse().unwrap(),
        },
    };
    let signed = node.sign(0, AdminCommand::ScheduleCorporateAction(action), NOW + 60_000);
    let count = node.entries().len();
    let mut rate = serde_json::to_value(&signed).unwrap();
    rate["command"]["adjustment"]["withholding_rate"] = serde_json::json!("0");
    let mut boundary = serde_json::to_value(&signed).unwrap();
    boundary["command"]["ex_open"] = serde_json::json!(NOW + 180_000);
    for altered in [rate, boundary] {
        let (status, body) = node.post("/v1/admin", altered).await;
        assert_eq!((status, body["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("invalid signature")));
    }
    let mut missing = serde_json::to_value(&signed).unwrap();
    missing["command"]["adjustment"].as_object_mut().unwrap().remove("withholding_rate");
    let request = HttpRequest::post("/v1/admin")
        .header("content-type", "application/json")
        .body(Body::from(missing.to_string()))
        .unwrap();
    let app = router(node.state.clone()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(std::str::from_utf8(&body).unwrap().contains("withholding_rate"));
    assert_eq!(node.entries().len(), count, "unverified or incomplete instructions stay out of the log");
}

#[tokio::test]
async fn corporate_actions_dev_scenario_signed_dividend_split_restart_and_replay() {
    let node = Node::start("corporate-actions").await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(serve(
        listener,
        node.tls.acceptors.clone(),
        router(node.state.clone()),
        node.state.limits.clone(),
        dev_config().api,
    ));
    let files = scratch_dir("corporate-cli-files");
    fs::create_dir_all(&files).unwrap();
    let mut spec = dev_config().genesis.markets.into_iter().find(|spec| spec.id == STOCK).unwrap();
    assert_eq!(spec.asset_class, AssetClass::Equity);
    spec.price_rules.max_age_ms = 600_000;
    spec.financing.markup_bps = 0;
    spec.schedule = serde_json::from_value(serde_json::json!({
        "kind": "weekly", "tz": "UTC",
        "windows": [{"open": "Tue 14:00", "close": "Tue 14:01"}],
        "special_sessions": [
            {"from": "2026-10-20T14:02:00", "to": "2026-10-20T14:03:00"},
            {"from": "2026-10-20T14:04:00", "to": "2026-10-20T14:05:00"}
        ]
    }))
    .unwrap();
    let listing = files.join("equity.toml");
    fs::write(&listing, toml::to_string(&spec).unwrap()).unwrap();
    cli(&node, &url, "upsert-market", &listing).await;
    for account in [TRADER, SHORT] {
        let (status, _) =
            node.post("/v1/dev/deposit", serde_json::json!({ "account": account, "amount": "10000" })).await;
        assert_eq!(status, StatusCode::OK);
    }
    let quotes = ["a", "b"]
        .map(|source| SourceQuote { source: source.into(), mid: Fixed::from_int(80), ts: NOW, contract: None })
        .to_vec();
    node.state.submit(Command::Prices { market: STOCK, quotes }).await.unwrap();
    for (account, side, qty) in [(TRADER, "buy", "2"), (SHORT, "sell", "3")] {
        let (_, events) = node
            .post("/v1/orders", order(account, side, qty, serde_json::json!({"kind":"market", "quoted":"80"})))
            .await;
        assert!(events.as_array().unwrap().iter().any(|e| e["type"] == "fill"), "{events}");
    }
    let (_, events) =
        node.post("/v1/orders", order(TRADER, "buy", "1", serde_json::json!({"kind":"limit", "price":"64"}))).await;
    assert!(events.as_array().unwrap().iter().any(|e| e["type"] == "order_accepted"), "{events}");
    let initial = replay(&node.entries()).unwrap().engine;
    let dividend = CorporateAction {
        id: "dev-dividend".into(),
        market: STOCK,
        previous_close: NOW + 60_000,
        ex_open: NOW + 120_000,
        adjustment: Adjustment::Dividend {
            gross_per_unit: Fixed::from_int(4),
            withholding_rate: "0.25".parse().unwrap(),
        },
    };
    let split = CorporateAction {
        id: "dev-split".into(),
        market: STOCK,
        previous_close: NOW + 180_000,
        ex_open: NOW + 240_000,
        adjustment: Adjustment::Split { new_units: 4, old_units: 1 },
    };
    for action in [dividend, split] {
        let file = files.join(format!("{}.toml", action.id));
        fs::write(&file, toml::to_string(&action).unwrap()).unwrap();
        let signed = cli(&node, &url, "schedule-corporate-action", &file).await;
        let duplicate = admin::submit(&node.api(&url), &signed).await.unwrap_err();
        assert!(duplicate.to_string().contains("409 Conflict"), "{duplicate:#}");
    }
    advance(&node, NOW + 120_000).await;
    let (_, long) = node.get(&format!("/v1/accounts/{TRADER}")).await;
    let (_, short) = node.get(&format!("/v1/accounts/{SHORT}")).await;
    assert_eq!(long["balance"], "10006");
    assert_eq!(short["balance"], "9988");
    let before = replay(&node.entries()).unwrap().engine;
    advance(&node, NOW + 240_000).await;
    let after = replay(&node.entries()).unwrap().engine;
    for account in [TRADER, SHORT] {
        let account: AccountId = account.parse().unwrap();
        let old = before.accounts()[&account].position(STOCK);
        let current = after.accounts()[&account].position(STOCK);
        assert_eq!(current.qty, old.qty.mul_int(4));
        assert_eq!(current.entry, old.entry.div_int(4));
        assert_eq!(
            current.qty.mul(after.markets()[&STOCK].mark.unwrap()),
            old.qty.mul(before.markets()[&STOCK].mark.unwrap())
        );
        assert_eq!(
            current.unrealised(after.markets()[&STOCK].mark.unwrap(), ProtocolVersion::V23),
            old.unrealised(before.markets()[&STOCK].mark.unwrap(), ProtocolVersion::V23)
        );
    }
    let trader: AccountId = TRADER.parse().unwrap();
    let resting = after.accounts()[&trader].orders.values().find(|o| o.market == STOCK).unwrap();
    assert_eq!((resting.qty, resting.kind), (Fixed::from_int(4), OrderKind::Limit { price: Fixed::from_int(16) }));
    assert_eq!(after.markets()[&STOCK].mark, Some(Fixed::from_int(20)));
    assert_eq!(after.markets()[&STOCK].client_cost, before.markets()[&STOCK].client_cost);
    let cash = |engine: &Engine| engine.accounts().values().map(|a| a.balance).sum::<Fixed>() + engine.house_cash();
    assert_eq!(cash(&initial), cash(&after));
    // The split's closure dropped the pre-split quotes, so the post-split order prices from a fresh
    // opening quote rather than a stale one.
    let quotes = ["a", "b"]
        .map(|source| SourceQuote {
            source: source.into(),
            mid: Fixed::from_int(20),
            ts: NOW + 240_000,
            contract: None,
        })
        .to_vec();
    node.state.submit(Command::Prices { market: STOCK, quotes }).await.unwrap();
    let (_, events) =
        node.post("/v1/orders", order(TRADER, "buy", "1", serde_json::json!({"kind":"market", "quoted":"20"}))).await;
    let fill = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "fill")
        .expect("the fresh opening quote fills the post-split order");
    assert_eq!(fill["price"], "20", "a market order before a new quote cannot fill at 80");
    assert_eq!(fill["position"], "9");
    let entries = node.entries();
    let logged = admin_entries(&entries);
    assert_eq!(
        logged
            .iter()
            .filter(|(request, _)| matches!(request.command, AdminCommand::ScheduleCorporateAction(_)))
            .count(),
        2
    );
    let dividend_at = entries.iter().position(|entry| matches!(entry, LogEntry::Command(c) if c.events.iter().flatten().any(|e| matches!(e, Event::CorporateDividend { .. })))).unwrap();
    let split_at = entries.iter().position(|entry| matches!(entry, LogEntry::Command(c) if c.events.iter().flatten().any(|e| matches!(e, Event::CorporateSplit { .. })))).unwrap();
    assert!(dividend_at < split_at);
    let mut replayed = replay(&entries).unwrap().engine;
    let root = replayed.state_root();
    server.abort();
    let _ = server.await;
    let Node { state, dir, stamps, key, task, .. } = node;
    drop(state);
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("sequencer stops after API shutdown")
        .unwrap()
        .unwrap();
    let (stream, _) = broadcast::channel(1024);
    let (withdrawals_tx, _) = mpsc::unbounded_channel();
    let (manifest_tx, _) = watch::channel(Manifest::default());
    let cfg = SequencerConfig { clock: stamps.clone(), ..sequencer_config(&dir, manifest_tx) };
    let mut recovered = Sequencer::recover(cfg, key, stream, withdrawals_tx).unwrap();
    recovered.trust(sample_around(stamps.as_ref())).unwrap();
    assert_eq!(recovered.state_root(), root, "the recovered sequencer holds the replayed state");
    let mut final_replay = replay(&log_entries(&dir)).unwrap().engine;
    assert_eq!(final_replay.state_root(), root, "restart preserves the public replay root");
    assert!(final_replay.corporate_actions().is_empty());
}

/// Runs the normal signed operator client against a local pinned TLS listener.
async fn cli(node: &Node, url: &str, action: &str, file: &std::path::Path) -> AdminRequest {
    let key = KeyEnv { key_file: None, key: Some(HexKey(format!("0x{}", hex::encode(DEV_KEY)))) };
    admin::run_with_clock(node.args(url, &[action, file.to_str().unwrap()]), key, || node.stamps.true_ms())
        .await
        .unwrap()
}

/// Moves trusted sequenced time without sleeping or changing a production clock.
async fn advance(node: &Node, at: Millis) {
    node.stamps.advance(Duration::from_millis((at - node.stamps.true_ms()) as u64));
    node.state.tx.send(Request::TimeSample(sample_around(node.stamps.as_ref()))).await.unwrap();
    node.state.submit(Command::Tick).await.unwrap();
}

/// The unsigned client order shape the development config authorizes.
fn order(account: &str, side: &str, qty: &str, kind: Value) -> Value {
    serde_json::json!({"account":account, "client_id":34, "market":STOCK, "side":side, "qty":qty, "kind":kind})
}
