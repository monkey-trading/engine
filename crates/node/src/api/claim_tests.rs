//! MONKEY claim proofs while the vault lags the node's checkpoints: the proof is served at the
//! checkpoint the vault accepted, whatever the node has created since.

use axum::http::StatusCode;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::emissions::{EmissionSchedule, PERIOD_MS, TokenUnits};
use engine_core::merkle::EntitlementProof;
use engine_core::types::{AccountId, Side};
use serde_json::{Value, json};
use std::time::Duration;

use super::proof_tests::{NOW, Node, account, logged_root};
use crate::checkpoints::LatestCheckpoint;
use crate::log::LogEntry;
use crate::sequencer::{ClaimAnswer, Request};

/// The account's route.
fn route(trader: AccountId) -> String {
    format!("/v1/accounts/{trader}/emissions")
}

/// Sequences a losing round trip, lets its period end, and checkpoints the allocation in; the
/// first call also starts the emission schedule.
async fn earn(node: &Node, trader: AccountId, first: bool) {
    if first {
        let schedule = EmissionSchedule { start_ms: NOW, period_budget: TokenUnits(1_000) };
        let (status, _) = node.post("/v1/admin", node.signed(AdminCommand::InitEmissions(schedule))).await;
        assert_eq!(status, StatusCode::OK);
        node.deposit(trader, "100000").await;
    }
    for (side, mid) in [(Side::Buy, "100000"), (Side::Sell, "99000")] {
        node.price_btc(mid).await;
        let order = OrderRequest {
            account: trader,
            client_id: 1,
            market: 1,
            side,
            qty: "0.01".parse().unwrap(),
            kind: OrderKind::Market { quoted: mid.parse().unwrap(), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        };
        assert_eq!(node.post("/v1/orders", serde_json::to_value(order).unwrap()).await.0, StatusCode::OK);
    }
    node.advance(Duration::from_millis(PERIOD_MS as u64)).await;
    // The first command after the period's end finalises it.
    node.state.submit(Command::Tick).await.expect("the sequencer is running");
    node.checkpoint().await;
}

/// Creates one more checkpoint.
async fn next_checkpoint(node: &Node) {
    node.state.submit(Command::Tick).await.expect("the sequencer is running");
    node.checkpoint().await;
}

/// Tells the sequencer the vault's latest checkpoint is the one at `seq`, as the poster does.
async fn vault_accepts(node: &Node, seq: u64) {
    report_latest(node, LatestCheckpoint { seq, claimable: true }).await;
}

/// Tells the sequencer the vault's latest checkpoint, as the poster does.
async fn report_latest(node: &Node, latest: LatestCheckpoint) {
    let status = Request::CheckpointStatus { posted: None, latest: Some(latest), health: None, lifecycle: None };
    node.state.tx.send(status).await.expect("the sequencer is running");
}

/// The seq of every checkpoint the node has logged, oldest first.
fn checkpoints(node: &Node) -> Vec<u64> {
    node.entries()
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(cp.seq),
            _ => None,
        })
        .collect()
}

/// The proof an answer carries, without the fields that say what it is proven against.
pub(super) fn proof_of(body: &Value) -> EntitlementProof {
    serde_json::from_value::<ClaimAnswer<EntitlementProof>>(body.clone()).unwrap().proof
}

#[tokio::test]
async fn a_lagging_vault_gets_the_proof_at_its_accepted_checkpoint_and_a_caught_up_one_the_newest() {
    let node = Node::start("claim-lagging").await;
    let trader = account(1);
    // Asking before anything is owed registers the account, so the checkpoints that follow keep its proof.
    assert_eq!(node.get(&route(trader)).await.0, StatusCode::NOT_FOUND);
    earn(&node, trader, true).await;
    let accepted = *checkpoints(&node).last().unwrap();
    vault_accepts(&node, accepted).await;
    next_checkpoint(&node).await;
    next_checkpoint(&node).await;
    let newest = *checkpoints(&node).last().unwrap();
    assert!(newest > accepted);

    let (status, body) = node.get(&route(trader)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let proof = proof_of(&body);
    assert_eq!(proof.seq, accepted, "the proof is at the vault's checkpoint, not the node's newest");
    assert!(proof.verify(logged_root(&node.entries(), accepted)));
    assert_eq!(proof.entitled, TokenUnits(1_000));
    assert_eq!(
        (body["accepted_seq"].as_u64(), body["newest_seq"].as_u64(), body["claimable"].as_bool()),
        (Some(accepted), Some(newest), Some(true)),
    );

    // The vault catches up: the newest checkpoint's proof is the accepted one.
    vault_accepts(&node, newest).await;
    let (_, body) = node.get(&route(trader)).await;
    let proof = proof_of(&body);
    assert_eq!(proof.seq, newest);
    assert!(proof.verify(logged_root(&node.entries(), newest)));
    assert_eq!((body["accepted_seq"].as_u64(), body["claimable"].as_bool()), (Some(newest), Some(true)));
}

#[tokio::test]
async fn an_entitlement_that_grew_after_the_accepted_root_is_proven_at_what_it_was_then() {
    let node = Node::start("claim-grown").await;
    let trader = account(1);
    assert_eq!(node.get(&route(trader)).await.0, StatusCode::NOT_FOUND);
    earn(&node, trader, true).await;
    let accepted = *checkpoints(&node).last().unwrap();
    vault_accepts(&node, accepted).await;
    earn(&node, trader, false).await;

    let (_, body) = node.get(&route(trader)).await;
    let proof = proof_of(&body);
    assert_eq!((proof.seq, proof.entitled), (accepted, TokenUnits(1_000)), "never more than the accepted root holds");
    assert_eq!(body["newest_entitled"], json!(TokenUnits(2_000)), "the answer says what the newest root holds");
    assert_eq!(body["claimable"], json!(true));
    assert!(proof.verify(logged_root(&node.entries(), accepted)));

    // Once the vault accepts the newer root, the whole entitlement is claimable.
    let newest = *checkpoints(&node).last().unwrap();
    vault_accepts(&node, newest).await;
    let proof = proof_of(&node.get(&route(trader)).await.1);
    assert_eq!((proof.seq, proof.entitled), (newest, TokenUnits(2_000)));
}

#[tokio::test]
async fn with_no_accepted_checkpoint_the_answer_is_the_newest_proof_and_waits() {
    let node = Node::start("claim-none-accepted").await;
    let trader = account(1);
    earn(&node, trader, true).await;
    let (status, body) = node.get(&route(trader)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let newest = *checkpoints(&node).last().unwrap();
    assert_eq!(proof_of(&body).seq, newest);
    assert_eq!(
        (body["accepted_seq"].clone(), body["newest_seq"].as_u64(), body["claimable"].as_bool()),
        (Value::Null, Some(newest), Some(false)),
    );

    // An account first asking after the vault's checkpoint passed has none retained for it: it waits
    // for the vault to reach the newest, and is claimable the moment it does.
    vault_accepts(&node, newest - 1).await;
    next_checkpoint(&node).await;
    let (_, body) = node.get(&route(trader)).await;
    assert_eq!(body["claimable"], json!(false));
    let newest = *checkpoints(&node).last().unwrap();
    vault_accepts(&node, newest).await;
    assert_eq!(node.get(&route(trader)).await.1["claimable"], json!(true));
}

#[tokio::test]
async fn a_restart_forgets_what_was_retained_and_waits_for_the_vault_again() {
    let node = Node::start("claim-restart").await;
    let trader = account(1);
    assert_eq!(node.get(&route(trader)).await.0, StatusCode::NOT_FOUND);
    earn(&node, trader, true).await;
    let accepted = *checkpoints(&node).last().unwrap();
    vault_accepts(&node, accepted).await;
    next_checkpoint(&node).await;
    assert_eq!(proof_of(&node.get(&route(trader)).await.1).seq, accepted);

    let node = node.restart().await;
    let newest = *checkpoints(&node).last().unwrap();
    let (status, body) = node.get(&route(trader)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let proof = proof_of(&body);
    assert_eq!(proof.seq, newest, "the tree is at the newest checkpoint again");
    assert!(proof.verify(logged_root(&node.entries(), newest)));
    assert_eq!((body["accepted_seq"].clone(), body["claimable"].as_bool()), (Value::Null, Some(false)));
    // The poster reports the vault's checkpoint again once it has read it.
    vault_accepts(&node, newest).await;
    assert_eq!(node.get(&route(trader)).await.1["claimable"], json!(true));
}

#[tokio::test]
async fn a_change_of_signer_ends_what_is_claimable_until_the_next_one_records_a_checkpoint() {
    let node = Node::start("claim-signer-change").await;
    let trader = account(1);
    assert_eq!(node.get(&route(trader)).await.0, StatusCode::NOT_FOUND);
    earn(&node, trader, true).await;
    let accepted = *checkpoints(&node).last().unwrap();
    vault_accepts(&node, accepted).await;
    assert_eq!(node.get(&route(trader)).await.1["claimable"], json!(true));
    next_checkpoint(&node).await;
    let newest = *checkpoints(&node).last().unwrap();

    // The signer that recorded the vault's latest is replaced or retired.
    report_latest(&node, LatestCheckpoint { seq: accepted, claimable: false }).await;
    let (_, body) = node.get(&route(trader)).await;
    assert_eq!(proof_of(&body).seq, newest, "the old epoch's root is no longer served");
    assert_eq!((body["accepted_seq"].clone(), body["claimable"].clone()), (Value::Null, json!(false)));

    // The checkpoint retained for the account is the next signer's first, whatever it names.
    vault_accepts(&node, newest).await;
    next_checkpoint(&node).await;
    let (_, body) = node.get(&route(trader)).await;
    let proof = proof_of(&body);
    assert_eq!((proof.seq, body["claimable"].clone()), (newest, json!(true)));
    assert!(proof.verify(logged_root(&node.entries(), newest)));
}
