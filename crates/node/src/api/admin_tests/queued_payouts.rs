//! `engine admin` against a node whose answer to a refused command ends with the withdrawals the queue
//! pays after its outcome.

use super::*;

use engine_core::Fixed;

#[tokio::test]
async fn engine_admin_fails_on_a_rejection_followed_by_the_withdrawals_it_pays() {
    // A house with no capital owes `TRADER` 1,000 once it sells. The trader withdraws every deposited
    // dollar, then queues twenty requests of 10 that wait for cash, which the house pays in: sixteen go
    // out with that payment and four with the next command, the one `engine admin` signs.
    let mut genesis = dev_config().genesis;
    genesis.house_capital = Fixed::ZERO;
    let trader: engine_core::types::AccountId = TRADER.parse().unwrap();
    let node = Node::start_with("cli-queued-payouts", false, &genesis).await;
    node.fund_and_buy_btc().await;
    let quotes = ["a", "b"]
        .map(|venue| SourceQuote { source: venue.into(), mid: "110000".parse().unwrap(), ts: NOW, contract: None })
        .to_vec();
    node.state.submit(Command::Prices { market: 1, quotes }).await.expect("the sequencer is running");
    let sell = serde_json::json!({
        "account": TRADER, "client_id": 2, "market": 1, "side": "sell", "qty": "0.1",
        "kind": { "kind": "market", "quoted": "110000" },
    });
    let (_, sold) = node.post("/v1/orders", sell).await;
    assert!(sold.as_array().unwrap().iter().any(|e| e["type"] == "fill"), "{sold}");
    let withdraw = |amount: i64| Command::Withdraw { account: trader, amount: Fixed::from_int(amount), auth: None };
    let paid = node.state.submit(withdraw(100_000)).await.expect("the sequencer is running");
    assert!(paid.iter().any(|e| matches!(e, Event::WithdrawalApproved { .. })), "{paid:?}");
    for _ in 0..20 {
        let queued = node.state.submit(withdraw(10)).await.expect("the sequencer is running");
        assert!(matches!(&queued[..], [Event::WithdrawalQueued { .. }]), "{queued:?}");
    }
    let capital = Command::HouseCapital { amount: Fixed::from_int(200), chain_ref: "house-in".into() };
    let events = node.state.submit(capital).await.expect("the sequencer is running");
    assert_eq!(events.iter().filter(|e| matches!(e, Event::WithdrawalApproved { .. })).count(), 16, "{events:?}");

    let url = format!("https://{}", node.serve().await);
    let key = || KeyEnv { key_file: None, key: Some(HexKey(format!("0x{}", hex::encode(DEV_KEY)))) };
    let err = admin::run_with_clock(node.args(&url, &["set-stress-floor", "9"]), key(), || NOW).await.unwrap_err();
    assert!(err.to_string().contains("rejected the command: stress level 9 does not exist"), "{err:#}");
    let logged = admin_entries(&node.entries());
    let [(_, events)] = &logged[..] else { panic!("{logged:?}") };
    assert!(matches!(events.first(), Some(Event::AdminRejected { .. })), "{events:?}");
    let approved = events.iter().filter(|e| matches!(e, Event::WithdrawalApproved { .. })).count();
    assert_eq!(approved, 4, "the outcome is followed by the withdrawals the command pays: {events:?}");
}
