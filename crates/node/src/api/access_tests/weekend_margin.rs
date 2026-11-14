//! The `V59` dev scenario: WTI's 7.5% weekend margin on a dev node, through the signed order and
//! withdrawal routes, the market and account reads, and the admin route over TLS, on deterministic
//! time. A WTI long at the 1% band (100x) opened mid-week cannot add from two hours before Friday's
//! 16:00 Chicago close, and is closed out at the last price before it when the weekend gap starts,
//! while a long that holds the weekend margin stays and may withdraw only what leaves the margin over
//! the gap; on Sunday's open the normal margin applies again; a calendar change that would narrow the
//! gap, and a relisting that moves the weekend margin without notice, are refused; and the public log
//! replays to the same state, which a restart recovers.

use chrono::NaiveDateTime;
use engine_core::command::AdminCommand;
use engine_core::market::MarketSpec;
use engine_core::session::{Period, Schedule};
use engine_core::types::MarketId;

use super::spreads::{Operator, fill_event, order_events, price};
use super::*;
use crate::clock::Clock;

const WTI: MarketId = 32;
/// Friday 23 October 2026, 16:00 in Chicago: the dev node's first WTI weekend close after `NOW`.
const CLOSE: i64 = 1_792_789_200_000;
/// Two hours before it, when the weekend margin starts to apply to opens and adds.
const FROM: i64 = CLOSE - 7_200_000;
/// Sunday 25 October 2026, 17:00 in Chicago: WTI's reopening.
const REOPEN: i64 = 1_792_965_600_000;

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_wti_meets_its_weekend_margin_from_two_hours_before_the_close() {
    let mut node = Node::start("weekend-margin-dev-scenario", true).await;
    let operator = Operator::serving(&node).await;
    let (a, b) = (key(0x81), key(0x82));
    for client in [&a, &b] {
        node.onboard(client).await;
    }
    let (a_id, b_id) = (eip712::address(a.verifying_key()), eip712::address(b.verifying_key()));

    // 1. The market read names WTI's weekend margin and when it next applies.
    let markets = node.get("/v1/markets").await;
    let wti = markets.as_array().unwrap().iter().find(|market| market["id"] == WTI).unwrap();
    assert_eq!(wti["weekend_margin_bps"], 750, "{wti}");
    let window = json!({ "weekend_margin_bps": 750, "from": FROM, "close": CLOSE, "reopen": REOPEN });
    assert_eq!(wti["weekend_margin"], window, "{wti}");

    // 2. Mid-week, each holding $100k, A opens $1.96M at the 1% band (100x) and B $700k; the account
    // read names the $147k A needs when the gap starts.
    price(&node, WTI, "70").await;
    for (client, qty) in [(&a, "28000"), (&b, "10000")] {
        fill_event(&node, client, WTI, Side::Buy, qty, "70").await;
    }
    let account = node.get(&format!("/v1/accounts/{a_id}")).await;
    let needs =
        json!({ "weekend_margin_bps": 750, "from": FROM, "close": CLOSE, "reopen": REOPEN, "required": "147000" });
    assert_eq!(account["weekend_margins"][WTI.to_string()], needs, "{account}");
    // The operator cannot split WTI's coming weekend gap with a minute's session on Saturday, which
    // would take its weekend margin off that weekend with no notice.
    let spec = node.state.query(|engine| engine.markets()[&WTI].spec.clone()).await.unwrap();
    let mut split = spec.clone();
    let Schedule::Weekly { special_sessions, .. } = &mut split.schedule else { unreachable!("weekly") };
    let at = |time: &str| NaiveDateTime::parse_from_str(time, "%Y-%m-%d %H:%M").unwrap();
    special_sessions.push(Period { from: at("2026-10-24 12:00"), to: at("2026-10-24 12:01") });
    let reason = "a calendar change may not narrow or remove market 32's weekend gap";
    operator.refuses(&node, AdminCommand::UpsertMarket(split), reason).await;

    // 3. A minute before the lead, A adds at the normal margin; from it, an add is refused: A would need
    // 7.5% of $1.974M.
    wait_until(&node, FROM - 60_000).await;
    price(&node, WTI, "70").await;
    fill_event(&node, &a, WTI, Side::Buy, "100", "70").await;
    wait_until(&node, FROM).await;
    price(&node, WTI, "70").await;
    let events = order_events(&node, &a, WTI, Side::Buy, "100", "70").await;
    let refused = events.as_array().unwrap().iter().find(|event| event["type"] == "order_rejected");
    let reason = &refused.unwrap_or_else(|| panic!("{events}"))["reason"];
    assert_eq!((reason["kind"].as_str(), &reason["required"]), (Some("weekend_margin"), &json!("148050")), "{events}");

    // 4. The last price before the close is 70.5. When the gap starts, A is closed out at it, and B,
    // which holds the weekend margin, keeps its long.
    wait_until(&node, CLOSE - 1_000).await;
    price(&node, WTI, "70.5").await;
    node.clock.advance(Duration::from_millis(2_000));
    node.retrust().await;
    let events = node.state.submit(Command::Tick).await.unwrap();
    let close_out =
        events.iter().position(|event| matches!(event, Event::CloseOut { account, .. } if *account == a_id));
    let Some(Event::CloseOut { equity, margin, .. }) = close_out.map(|at| &events[at]) else { panic!("{events:?}") };
    assert!(*equity < *margin && *margin == "148578.75".parse::<Fixed>().unwrap(), "7.5% of $1.98105M: {events:?}");
    let fills: Vec<_> =
        events.iter().filter_map(|event| if let Event::Fill(fill) = event { Some(fill) } else { None }).collect();
    let [closed] = fills[..] else { panic!("one close-out fill: {events:?}") };
    assert!(closed.liquidation && closed.account == a_id && closed.market == WTI, "{closed:?}");
    assert_eq!((closed.index, closed.position), ("70.5".parse().unwrap(), Fixed::ZERO));
    let account = node.get(&format!("/v1/accounts/{b_id}")).await;
    assert_eq!(account["positions"][WTI.to_string()]["qty"], "10000", "{account}");
    let markets = node.get("/v1/markets").await;
    let wti = markets.as_array().unwrap().iter().find(|market| market["id"] == WTI).unwrap();
    assert_eq!(wti["weekend_margin"], window, "{wti}");

    // 5. On Saturday, B may take out only what leaves it 7.5% of its $705k, $52,875: about $51.5k of
    // its equity after the week's financing.
    wait_until(&node, CLOSE + 20 * 3_600_000).await;
    let events = withdraw(&node, &b, 60_000).await;
    let account = node.get(&format!("/v1/accounts/{b_id}")).await;
    assert_eq!(account["weekend_margins"][WTI.to_string()]["required"], "52875", "{account}");
    let fixed = |value: &Value| value.as_str().unwrap().parse::<Fixed>().unwrap();
    let available = fixed(&account["equity"]) - "52875".parse::<Fixed>().unwrap();
    assert_eq!(events[0]["reason"]["kind"], "insufficient_funds", "{events}");
    assert_eq!(fixed(&events[0]["reason"]["available"]), available, "{events} {account}");
    assert!(available > Fixed::from_int(51_000) && available < Fixed::from_int(52_000), "{available}");

    // 6. At Sunday's open the normal margin applies again: B takes out $60k, and A opens the same
    // $1.97M at the 1% band. The window moves a week on, to a reopening an hour later in UTC, as
    // Chicago's clocks go back on Sunday 1 November.
    wait_until(&node, REOPEN + 60_000).await;
    price(&node, WTI, "70.5").await;
    let events = withdraw(&node, &b, 60_000).await;
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "withdrawal_approved"), "{events}");
    fill_event(&node, &a, WTI, Side::Buy, "28000", "70.5").await;
    let (from, close, reopen) = (FROM + WEEK_MS, CLOSE + WEEK_MS, REOPEN + WEEK_MS + 3_600_000);
    let next = json!({ "weekend_margin_bps": 750, "from": from, "close": close, "reopen": reopen });
    let markets = node.get("/v1/markets").await;
    let wti = markets.as_array().unwrap().iter().find(|market| market["id"] == WTI).unwrap();
    assert_eq!(wti["weekend_margin"], next, "{wti}");

    // 7. The operator cannot move the weekend margin without a seven-day notice.
    let raised = MarketSpec { weekend_margin_bps: 1_000, ..spec };
    let reason = "market 32's weekend margin change needs a notice of at least seven days";
    operator.refuses(&node, AdminCommand::UpsertMarket(raised), reason).await;

    // 8. The public log, B's signed withdrawal included, replays against the enclave's key to the same
    // state, and a restart recovers the root it replays to.
    let domain = crate::log::CheckpointDomain { chain_id: 31_337, vault: VAULT };
    let signer = AccountId(EnclaveKey::test_key().address());
    let mut replayed = replay_with_signer(&node.entries(), domain, signer).unwrap().engine;
    let live = node.state.query(move |engine| [a_id, b_id].map(|id| engine.accounts()[&id].clone())).await.unwrap();
    assert_eq!([a_id, b_id].map(|id| replayed.accounts()[&id].clone()), live);
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

const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Moves the node's time forward to `at`, with the screening source observing at it.
async fn wait_until(node: &Node, at: i64) {
    let ms = at - node.clock.now_ms();
    node.move_time(u64::try_from(ms).expect("time moves forward")).await;
    node.source.lock().unwrap().observed_at = node.clock.now_ms();
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
