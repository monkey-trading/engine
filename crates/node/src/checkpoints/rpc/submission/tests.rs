//! Submission identity, restart, authorization, and durability regressions.

use std::fs;
use std::path::Path;
use std::time::Duration;

use alloy::primitives::Address;
use engine_core::Fixed;
use engine_core::types::AccountId;
use serde_json::{Value, json};

use crate::checkpoints::rpc::submission::SubmissionStore;
use crate::checkpoints::{CheckpointChain, CheckpointRpc, Poster, Work};
use crate::config::{CheckpointSection, FinalityTag};
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent};
use crate::tee::EnclaveKey;
use crate::test_support::scratch_dir;

mod fakes;
mod lifecycle;
mod nonce_progress;
use crate::checkpoints::rpc::submission::tests::fakes::Server;

#[tokio::test]
async fn long_pending_survives_restart_without_another_send_or_finality_release() {
    let dir = scratch_dir("submission-long-pending");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let cp = checkpoint(10);
    let chain = rpc(&server.url, &dir);
    chain.post(&cp).await.unwrap();
    let record = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    for _ in 0..40 {
        chain.post(&cp).await.unwrap();
    }
    drop(chain);
    let chain = rpc(&server.url, &dir);
    for _ in 0..40 {
        chain.post(&cp).await.unwrap();
    }
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), record);
    assert_eq!(server.state.lock().unwrap().sent.len(), 1, "a known pending transaction is only polled");
    let poster = Poster {
        registry: None,
        chain,
        domain: domain(),
        signer: AccountId(EnclaveKey::test_key().address()),
        retry: Duration::from_millis(10),
        pending_deposits_critical_after: Duration::from_secs(1),
    };
    let work = Work { checkpoint: cp.clone(), liabilities: Fixed::ZERO, deposits: 0, authorized: Fixed::ZERO };
    assert!(poster.reconcile(&work).await.is_err(), "pending is not canonical inclusion");
    server.state.lock().unwrap().latest = Some(cp.clone());
    assert_eq!(poster.reconcile(&work).await.unwrap(), (None, false), "latest inclusion cannot release withdrawals");
    server.state.lock().unwrap().finalized = Some(cp);
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(10), true));
    assert_eq!(server.state.lock().unwrap().sent.len(), 1);
}

#[tokio::test]
async fn lost_accepted_response_and_restart_resend_exact_call_at_reserved_nonce() {
    let dir = scratch_dir("submission-lost-response");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    server.state.lock().unwrap().lose_response = true;
    let cp = checkpoint(10);
    let chain = rpc(&server.url, &dir);
    assert!(chain.post(&cp).await.is_err());
    let record = read_record(&dir);
    assert_eq!(record["nonce"], 7);
    assert_eq!(record["checkpoint"], serde_json::to_value(&cp).unwrap());
    assert_eq!(record["transaction_hash"], Value::Null);
    drop(chain);
    // The pending RPC count is already 8; recovery must not reserve that next nonce.
    let chain = rpc(&server.url, &dir);
    chain.post(&cp).await.unwrap();
    chain.post(&cp).await.unwrap();
    let state = server.state.lock().unwrap();
    assert_eq!(state.sent.len(), 2);
    assert_eq!(state.sent[0], state.sent[1], "response loss resends identical nonce, chain, sender, vault and call");
    assert_eq!(state.sent[0]["nonce"], "0x7");
    assert_eq!(state.sent[0]["chainId"], "0x7a69");
    assert_eq!(read_record(&dir)["nonce"], 7);
}

#[tokio::test]
async fn dropped_transaction_reuses_nonce_but_consumed_nonce_never_allocates_another() {
    let dir = scratch_dir("submission-dropped-consumed");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let cp = checkpoint(10);
    let chain = rpc(&server.url, &dir);
    chain.post(&cp).await.unwrap();
    server.state.lock().unwrap().known = false;
    chain.post(&cp).await.unwrap();
    {
        let mut state = server.state.lock().unwrap();
        assert_eq!(state.sent.len(), 2);
        assert_eq!(state.sent[0], state.sent[1]);
        state.latest_nonce = 8;
    }
    assert!(chain.post(&cp).await.unwrap_err().to_string().contains("consumed"));
    assert_eq!(server.state.lock().unwrap().sent.len(), 2);
    assert_eq!(read_record(&dir)["nonce"], 7);
}

#[tokio::test]
async fn pending_owner_blocks_siblings_and_new_work_until_finalized_progress() {
    let dir = scratch_dir("submission-competing-work");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let chain = rpc(&server.url, &dir);
    let cp = checkpoint(10);
    chain.post(&cp).await.unwrap();
    let record = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    assert!(chain.post(&checkpoint(9)).await.is_err());
    assert!(chain.post(&checkpoint(11)).await.unwrap_err().to_string().contains("still pending"));
    let mut sibling = cp.clone();
    sibling.deferred_withdrawals = !cp.deferred_withdrawals;
    assert!(chain.post(&sibling).await.is_err());
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), record);
    assert_eq!(server.state.lock().unwrap().sent.len(), 1);
    let successor = nonce_progress::successor(&cp, 11);
    {
        let mut state = server.state.lock().unwrap();
        state.latest_nonce = 8;
        state.latest = Some(cp.clone());
        state.finalized = Some(cp.clone());
        state.consumer = Some(state.sent[0].clone());
    }
    chain.post(&successor).await.unwrap();
    assert_eq!(read_record(&dir)["nonce"], 8);
    assert_eq!(read_record(&dir)["checkpoint"]["seq"], 11);
    chain.post(&cp).await.unwrap();
    assert_eq!(read_record(&dir)["checkpoint"]["seq"], 11, "latest inclusion never replaces the successor");
    assert_eq!(server.state.lock().unwrap().sent.len(), 2);
}

#[tokio::test]
async fn external_pending_work_and_exhausted_nonce_refuse_a_reservation() {
    let dir = scratch_dir("submission-foreign-pending");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let chain = rpc(&server.url, &dir);
    server.state.lock().unwrap().pending_nonce = 8;
    assert!(chain.post(&checkpoint(10)).await.unwrap_err().to_string().contains("unreserved"));
    assert!(!dir.join("checkpoint-submission.json").exists());
    {
        let mut state = server.state.lock().unwrap();
        state.latest_nonce = u64::MAX;
        state.pending_nonce = u64::MAX;
    }
    assert!(chain.post(&checkpoint(10)).await.unwrap_err().to_string().contains("exhausted"));
    assert!(server.state.lock().unwrap().sent.is_empty());
}

#[tokio::test]
async fn locking_and_storage_failure_prevent_sends_and_leave_owned_record_intact() {
    let dir = scratch_dir("submission-lock-storage");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let chain = rpc(&server.url, &dir);
    let store =
        SubmissionStore::load(&dir, domain(), Address::repeat_byte(3), AccountId(EnclaveKey::test_key().address()))
            .unwrap();
    let lock = store.lock().unwrap();
    assert!(rpc(&server.url, &dir).post(&checkpoint(10)).await.unwrap_err().to_string().contains("lock"));
    assert!(!dir.join("checkpoint-submission.json").exists());
    drop(lock);
    // A staging-path failure must happen before eth_sendTransaction.
    fs::create_dir(dir.join("checkpoint-submission.json.tmp")).unwrap();
    assert!(chain.post(&checkpoint(10)).await.is_err());
    assert!(server.state.lock().unwrap().sent.is_empty());
    fs::remove_dir(dir.join("checkpoint-submission.json.tmp")).unwrap();
    chain.post(&checkpoint(10)).await.unwrap();
    let bytes = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    // A pre-rename crash can leave arbitrary staging bytes; the owned record remains authoritative.
    fs::write(dir.join("checkpoint-submission.json.tmp"), b"partial").unwrap();
    server.state.lock().unwrap().known = false;
    rpc(&server.url, &dir).post(&checkpoint(10)).await.unwrap();
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    assert!(!dir.join("checkpoint-submission.json.tmp").exists());
    assert_eq!(server.state.lock().unwrap().sent.len(), 2);
}

#[test]
fn startup_and_unauthorized_checkpoint_validation_preserve_storage_bytes() {
    let dir = scratch_dir("submission-startup-read-only");
    let _ = rpc("http://127.0.0.1:1", &dir);
    assert!(!dir.exists(), "construction must not even create a directory");
    fs::create_dir_all(&dir).unwrap();
    let store =
        SubmissionStore::load(&dir, domain(), Address::repeat_byte(3), AccountId(EnclaveKey::test_key().address()))
            .unwrap();
    let record = store.reserve(checkpoint(10), 7, None).unwrap();
    let valid = serde_json::to_value(record).unwrap();
    let mut cases = Vec::new();
    for (field, value) in
        [("version", json!(2)), ("nonce", json!(u64::MAX)), ("relayer", json!(Address::repeat_byte(4)))]
    {
        let mut bad = valid.clone();
        bad[field] = value;
        cases.push(serde_json::to_vec(&bad).unwrap());
    }
    let mut bad_domain = valid.clone();
    bad_domain["domain"]["chain_id"] = json!(1);
    cases.push(serde_json::to_vec(&bad_domain).unwrap());
    let mut forged = valid.clone();
    forged["checkpoint"]["root"] = json!(format!("0x{}", "ff".repeat(32)));
    cases.push(serde_json::to_vec(&forged).unwrap());
    cases.push(b"{incomplete".to_vec());
    cases.push(vec![b' '; 4097]);
    for bytes in cases {
        fs::write(dir.join("checkpoint-submission.json"), &bytes).unwrap();
        assert!(
            SubmissionStore::load(&dir, domain(), Address::repeat_byte(3), AccountId(EnclaveKey::test_key().address()))
                .is_err()
        );
        assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }
    fs::write(dir.join("checkpoint-submission.json"), serde_json::to_vec(&valid).unwrap()).unwrap();
    assert!(
        SubmissionStore::load(&dir, domain(), Address::repeat_byte(3), AccountId([9; 20])).is_err(),
        "stored signer never supplies authority"
    );
}

#[tokio::test]
async fn unauthorized_checkpoint_refuses_before_creating_lock_or_reservation() {
    let dir = scratch_dir("submission-unauthorized");
    let chain = rpc("http://127.0.0.1:1", &dir);
    let mut cp = checkpoint(10);
    cp.root = format!("0x{}", "ff".repeat(32));
    assert!(chain.post(&cp).await.is_err());
    assert!(!dir.exists());
}

#[tokio::test]
async fn known_hash_must_match_reserved_transaction_fields() {
    let dir = scratch_dir("submission-lookup-identity");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let chain = rpc(&server.url, &dir);
    let cp = checkpoint(10);
    chain.post(&cp).await.unwrap();
    let transaction = server.state.lock().unwrap().sent[0].clone();
    let bytes = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    for (field, value) in [
        ("nonce", json!("0x8")),
        ("from", json!(Address::repeat_byte(4))),
        ("to", json!(Address::repeat_byte(4))),
        ("chainId", json!("0x1")),
        ("input", json!("0x00")),
        ("value", json!("0x1")),
    ] {
        let mut wrong = transaction.clone();
        wrong[field] = value;
        server.state.lock().unwrap().lookup_override = Some(wrong);
        assert!(chain.post(&cp).await.unwrap_err().to_string().contains("another transaction"), "{field}");
        assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
        assert_eq!(server.state.lock().unwrap().sent.len(), 1);
    }
    // Clients may spell equivalent calldata as data or input.
    let mut alias = transaction.clone();
    alias["data"] = alias.as_object_mut().unwrap().remove("input").unwrap();
    server.state.lock().unwrap().lookup_override = Some(alias);
    chain.post(&cp).await.unwrap();
    assert_eq!(server.state.lock().unwrap().sent.len(), 1);
}

#[tokio::test]
async fn hash_store_failure_after_send_preserves_nonce_for_restart() {
    let dir = scratch_dir("submission-hash-store-failure");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    server.state.lock().unwrap().block_hash_store = true;
    let chain = rpc(&server.url, &dir);
    let cp = checkpoint(10);
    assert!(chain.post(&cp).await.is_err());
    assert_eq!(read_record(&dir)["nonce"], 7);
    assert_eq!(read_record(&dir)["transaction_hash"], Value::Null);
    drop(chain);
    fs::remove_dir(dir.join("checkpoint-submission.json.tmp")).unwrap();
    server.state.lock().unwrap().block_hash_store = false;
    rpc(&server.url, &dir).post(&cp).await.unwrap();
    let state = server.state.lock().unwrap();
    assert_eq!(state.sent.len(), 2);
    assert_eq!(state.sent[0], state.sent[1]);
}

#[tokio::test]
async fn changed_rpc_chain_refuses_before_reserving_or_sending() {
    let dir = scratch_dir("submission-rpc-chain-change");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let chain = rpc(&server.url, &dir);
    server.state.lock().unwrap().chain_id = 1;
    assert!(chain.post(&checkpoint(10)).await.unwrap_err().to_string().contains("another chain"));
    assert!(!dir.join("checkpoint-submission.json").exists());
    assert!(server.state.lock().unwrap().sent.is_empty());
}

// ── Helpers ──

/// Returns the deployment independently trusted by the fixture.
fn domain() -> CheckpointDomain {
    CheckpointDomain { chain_id: 31337, vault: AccountId([1; 20]) }
}

/// Signs one exact v2 checkpoint under the fixture's retained authority.
fn checkpoint(seq: u64) -> Checkpoint {
    let key = EnclaveKey::test_key();
    let parent = CheckpointParent::default();
    Checkpoint {
        parent: Some(parent),
        seq,
        root: format!("0x{}", "11".repeat(32)),
        signature: key.sign_checkpoint_v2(domain().chain_id, &domain().vault.0, seq, &[0x11; 32], parent),
        signer: key.address_hex(),
        domain: Some(domain()),
        deferred_withdrawals: true,
    }
}

/// Builds the actual RPC adapter, independently pinning its fixture authority.
fn rpc(url: &str, dir: &Path) -> CheckpointRpc {
    rpc_with_finality(url, dir, FinalityTag::Finalized, 0)
}

/// Builds the real adapter with the fixture's requested custody finality policy.
fn rpc_with_finality(url: &str, dir: &Path, finality: FinalityTag, confirmations: u64) -> CheckpointRpc {
    let cfg: CheckpointSection = toml::from_str(&format!(
        "rpc_url = {url:?}\nrelayer = '{:#x}'\nretry_interval = '10ms'\nrpc_timeout = '1s'",
        Address::repeat_byte(3)
    ))
    .unwrap();
    CheckpointRpc::new(
        &cfg,
        domain(),
        Address::repeat_byte(2),
        finality,
        confirmations,
        dir,
        AccountId(EnclaveKey::test_key().address()),
    )
    .unwrap()
}

/// Reads the actual durable reservation rather than a mock copy.
fn read_record(dir: &Path) -> Value {
    serde_json::from_slice(&fs::read(dir.join("checkpoint-submission.json")).unwrap()).unwrap()
}
