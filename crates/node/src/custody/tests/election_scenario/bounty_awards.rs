//! The bug bounty awards dev scenario, from `V89`, which `scripts/dev-bounty-awards.sh` runs on the
//! parameter votes scenario's stack (`parameter_votes::adopted_set`), with the checkpoint worker posting to
//! the vault: A, B and C, the rotation set at 40:30:30, are the stakers. A proposes a High award to a
//! researcher and B a Low award to another, and the set passes both, and a second vote for the High award
//! too; the operator's own award, a fast-path award and an award for another amount are refused. Seven
//! days after they are logged the awards apply, oldest first in the queue, and the second award for the
//! High award's advisory is refused. House earnings of 600,000 and 300,000 USDC above the floor
//! then close two epochs, each paying the front award 5% of its excess before the operator's share and
//! the stakers; a client's win puts the house below the floor, and that close pays nothing; the next
//! close finishes the first award and pays the second. Each researcher claims through the vault, every
//! answer of `GET /v1/awards` matches the queue, and the log replays.

use alloy::primitives::{Address, U256};
use axum::http::StatusCode;
use engine_core::Fixed;
use engine_core::awards::{AwardApproval, AwardPaid, Severity};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Millis, Side};
use k256::ecdsa::SigningKey;
use serde_json::{Value, json};

use super::node::Node;
use super::operator_share::{FLOOR_UNITS, TRADER_KEY, claim, declare, next_epoch, round_trip};
use super::parameter_votes::{NOTICE_MS, SET_EFFECTIVE_SECS, adopted_set, house_state, propose};
use crate::sequencer::replay_path;

/// One USDC in USDC base units.
const USDC: u128 = 1_000_000;

#[tokio::test]
#[ignore = "requires Anvil, Python 3, and compiled contracts; scripts/dev-bounty-awards.sh runs it"]
async fn dev_scenario_voted_awards_are_paid_from_closes_first_and_claimed_from_the_vault() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
    );
    let (mut chain, mut node, [a, b, c], operator) = adopted_set(true).await;
    let (researcher, second, trader) = (chain.accounts[6], chain.accounts[7], chain.accounts[5]);
    let trader_key = SigningKey::from_slice(&hex::decode(TRADER_KEY).unwrap()).unwrap();
    // The house holds the floor exactly, final, and a client a million, both through the vault.
    chain.fund(None, U256::from(FLOOR_UNITS)).await;
    chain.fund(Some(trader), U256::from(FLOOR_UNITS)).await;
    declare(&node, &trader_key).await;
    let client = AccountId(trader.into());
    node.wait("the deposits to credit and finalise", move |e| {
        e.house_cash() == Fixed::from_int(1_000_000)
            && e.accounts().get(&client).is_some_and(|account| {
                account.balance == Fixed::from_int(1_000_000) && account.unfinalised_deposits == Fixed::ZERO
            })
    })
    .await;
    node.advance_to((SET_EFFECTIVE_SECS as Millis + 60) * 1000).await;
    chain.mine_at(SET_EFFECTIVE_SECS + 30).await;

    // 1. A proposes a High award to the researcher and B a Low award to another; the set passes both.
    let high = award(researcher, Severity::High, "GHSA-4v2c-9xq8-jm3w");
    let (_, logged) = propose(&mut chain, &node, a, &high, false, &[b]).await;
    let low = award(second, Severity::Low, "GHSA-7rq2-c6jx-pm9f");
    propose(&mut chain, &node, b, &low, false, &[a]).await;
    // C proposes the High award again, as a second member unaware of A's would, and the set passes it.
    let (again, _) = propose(&mut chain, &node, c, &high, false, &[a]).await;
    let pending = house_state(&node).await["governance"]["pending"].clone();
    assert_eq!(pending.as_array().map(Vec::len), Some(3), "{pending}");
    assert_eq!(pending[0]["applies_at"], json!(logged + NOTICE_MS), "{pending}");
    assert_eq!(pending[2]["proposal"], json!(again), "{pending}");
    println!("PASS: A's High award, B's Low award and C's second High award passed with 70% and wait seven days");

    // 5. The operator's own award is refused at the route; a fast-path award and an award for the
    //    Critical amount at High pass on chain and the engine refuses both.
    let (status, body) = node.admin(&operator, high.clone()).await;
    assert_eq!((status, body), (StatusCode::BAD_REQUEST, json!({ "error": "the operator may not send this command" })));
    let (fast, _) = propose(&mut chain, &node, a, &high, true, &[b]).await;
    let AdminCommand::ApproveAward(approval) = &high else { unreachable!() };
    let inflated =
        AdminCommand::ApproveAward(AwardApproval { amount: Severity::Critical.amount(), ..approval.clone() });
    let (wrong, _) = propose(&mut chain, &node, a, &inflated, false, &[b]).await;
    let governance = node.query(|e| e.governance().clone()).await;
    assert!(governance.restrictions.is_empty(), "the fast path holds nothing: {governance:?}");
    let waiting: Vec<u64> = governance.pending.iter().map(|change| change.proposal).collect();
    assert!(!waiting.contains(&fast) && !waiting.contains(&wrong) && waiting.len() == 3, "{waiting:?}");
    println!("PASS: the operator's own award, a fast-path award and an award at another amount were refused");

    // 2. Seven days after they are logged both apply, the High award at the front of the queue.
    node.advance_to(logged + NOTICE_MS - 60_000).await;
    node.submit(Command::Tick).await;
    assert!(node.query(|e| e.awards().is_empty()).await, "nothing applies before its time");
    node.advance_to(logged + NOTICE_MS + 60_000).await;
    node.submit(Command::Tick).await;
    node.wait("every passed change to apply or be refused", |e| e.governance().pending.is_empty()).await;
    assert_eq!(
        node.query(|e| e.awards().len()).await,
        2,
        "the second vote for the High award's advisory applied nothing"
    );
    let view = awards(&node).await;
    assert_eq!(ids(&view["queue"]), [0, 1]);
    assert_eq!((&view["queue"][0]["recipient"], &view["queue"][0]["owed"]), (&json!(hex(researcher)), &units(50_000)));
    assert_eq!((&view["queue"][1]["severity"], &view["queue"][1]["owed"]), (&json!("low"), &units(1_000)));
    println!(
        "PASS: seven days after the votes both awards applied, the High award first, and the second award for its advisory was refused"
    );

    // 3. The house earns 600,000 USDC above the floor: the close pays the High award 5%, 30,000, first.
    let catch_up = next_epoch(&mut node, &chain).await;
    assert!(!catch_up.iter().any(|e| matches!(e, Event::AwardPaid(_))), "nothing above the floor: {catch_up:?}");
    chain.fund(None, U256::from(600_000 * USDC)).await;
    excess(&node, 600_000).await;
    let close = next_epoch(&mut node, &chain).await;
    assert_eq!(paid(&close), [(0, 30_000 * USDC, 20_000 * USDC)], "{close:?}");
    assert_eq!(awarded(&close), 30_000 * USDC);
    let share: u128 = close.iter().filter_map(operator_share).sum();
    assert_eq!(share, (600_000 - 30_000) * USDC / 5, "the operator's fifth is of what the award leaves");
    // The next earns 300,000: 15,000 more to the same award, and the Low award still waits behind it.
    chain.fund(None, U256::from(300_000 * USDC)).await;
    excess(&node, 300_000).await;
    let close = next_epoch(&mut node, &chain).await;
    assert_eq!(paid(&close), [(0, 15_000 * USDC, 5_000 * USDC)], "{close:?}");
    let view = awards(&node).await;
    assert_eq!((&view["queue"][0]["paid"], &view["queue"][0]["owed"]), (&units(45_000), &units(5_000)));
    assert_eq!(view["queue"][1]["paid"], units(0));
    println!("PASS: two closes paid the High award 30,000 and 15,000, each 5% of the excess, and the Low award waited");

    // 6. The client wins a short round trip, so the house is below the floor: the close pays nothing.
    round_trip(&node, &trader_key, Side::Sell, "1", "100900", "100000").await;
    let close = next_epoch(&mut node, &chain).await;
    let closed = close.iter().find_map(|e| match e {
        Event::DistributionEpochClosed { nav, floor, allocated, .. } => Some((*nav < *floor, allocated.0)),
        _ => None,
    });
    assert_eq!(closed, Some((true, 0)), "{close:?}");
    assert!(paid(&close).is_empty(), "{close:?}");
    assert_eq!(awards(&node).await["queue"][0]["owed"], units(5_000));
    println!("PASS: with the house below the floor the close paid no award");

    // 3, continued. The house earns 300,000 again, less the client's win: the close finishes the High
    //    award and pays the Low one in full from what the cap leaves.
    chain.fund(None, U256::from(300_000 * USDC)).await;
    node.wait("the house deposit to finalise", |e| e.distributable_excess().is_some_and(|x| x > Fixed::ZERO)).await;
    let close = next_epoch(&mut node, &chain).await;
    assert_eq!(paid(&close), [(0, 5_000 * USDC, 0), (1, 1_000 * USDC, 0)], "{close:?}");
    let view = awards(&node).await;
    assert_eq!((ids(&view["queue"]), ids(&view["paid"])), (vec![], vec![0, 1]), "{view}");
    assert_eq!(view["paid_total"], units(51_000));
    println!("PASS: the next close finished the High award and paid the Low award, oldest first");

    // 4. Each researcher claims through the vault against a posted root, relayed by another account.
    assert_eq!(claim(&chain, &node, researcher, UsdcUnits(50_000 * USDC)).await, U256::from(50_000 * USDC));
    assert_eq!(claim(&chain, &node, second, UsdcUnits(1_000 * USDC)).await, U256::from(1_000 * USDC));
    println!("PASS: the researchers claimed 50,000 and 1,000 USDC from the vault with distribution proofs");

    // 7. The log replays to its roots, with the same awards and distributions.
    let live = node.query(move |e| (e.awards().clone(), e.distribution(AccountId(researcher.into())))).await;
    let dir = node.dir.clone();
    node.stop().await;
    let replayed = replay_path(&dir).expect("the log replays");
    assert_eq!((replayed.engine.awards().clone(), replayed.engine.distribution(AccountId(researcher.into()))), live);
    println!("PASS: the log replays to its roots, with the same awards");
}

/// An award to `recipient` at the scale's amount for `severity`.
fn award(recipient: Address, severity: Severity, advisory: &str) -> AdminCommand {
    AdminCommand::ApproveAward(AwardApproval {
        recipient: AccountId(recipient.into()),
        severity,
        advisory: advisory.into(),
        amount: severity.amount(),
    })
}

/// Waits for the house to be `usd` above the floor, with every deposit final.
async fn excess(node: &Node, usd: i64) {
    let wanted = Fixed::from_int(usd);
    node.wait(&format!("the house to stand {usd} above the floor"), move |e| e.distributable_excess() == Some(wanted))
        .await;
}

/// `GET /v1/awards`.
async fn awards(node: &Node) -> Value {
    let (status, body) = node.get("/v1/awards").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The award ids a list of `GET /v1/awards` holds.
fn ids(list: &Value) -> Vec<u64> {
    list.as_array().expect("a list").iter().map(|award| award["id"].as_u64().expect("an id")).collect()
}

/// `usdc` whole USDC as the API writes base units.
fn units(usdc: u128) -> Value {
    json!((usdc * USDC).to_string())
}

fn hex(address: Address) -> String {
    format!("{address:#x}")
}

/// Each award a close's events paid: its id, the amount and what it still owes.
fn paid(events: &[Event]) -> Vec<(u64, u128, u128)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::AwardPaid(AwardPaid { award, amount, owed, .. }) => Some((*award, amount.0, owed.0)),
            _ => None,
        })
        .collect()
}

/// What the paying close among `events` recorded as awarded.
fn awarded(events: &[Event]) -> u128 {
    events
        .iter()
        .find_map(|e| match e {
            Event::DistributionEpochClosed { awarded, excess, .. } if excess.0 > 0 => Some(awarded.0),
            _ => None,
        })
        .expect("a close distributed")
}

fn operator_share(event: &Event) -> Option<u128> {
    match event {
        Event::OperatorShareAllocated { amount, .. } => Some(amount.0),
        _ => None,
    }
}
