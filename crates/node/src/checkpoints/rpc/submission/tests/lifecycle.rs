//! Real RPC pinning and retired reservation authority across signer epochs.

use alloy::primitives::{B256, U256};
use alloy::sol_types::SolCall;

use super::*;
use crate::checkpoints::rpc::{
    checkpointEpochAtCall, checkpointRootsCall, checkpointSignerAtCall, signerAtEpochCall, usedNoncesCall,
};
use crate::checkpoints::{ApprovalId, ApprovalStatus, View};

#[tokio::test]
async fn collateral_paid_epoch_nonce_and_history_reads_use_the_same_canonical_finalized_hash() {
    let dir = scratch_dir("payout-pinned-rpc");
    let server = Server::start(&dir).await;
    let cp = checkpoint(10);
    let approval = ApprovalId { account: AccountId([4; 20]), nonce: 7, approved_seq: 9 };
    {
        let mut state = server.state.lock().unwrap();
        state.latest = Some(cp.clone());
        state.finalized = Some(cp.clone());
        state.paid = U256::from(9_000_000);
        state.finalized_paid = Some(U256::from(3_000_001));
        state.epoch = 3;
        state.finalized_epoch = Some(2);
        let signer = state.signer;
        state.epoch_signers.insert(2, signer);
        state.balance = U256::from(70_000_000);
        state.claimed.insert((Address::from(approval.account.0), approval.nonce));
        state.accepted.insert(cp.seq, (cp.clone(), 1));
    }
    let chain = rpc(&server.url, &dir);
    let at = chain.observe(View::Finalized, &[approval]).await.unwrap();
    assert_eq!(at.state.height, 8);
    assert_eq!(at.state.balance, Fixed::from_int(70));
    let lifecycle = at.lifecycle.as_ref().unwrap();
    assert_eq!(lifecycle.total_withdrawn, Fixed::from_raw(3_000_001_000));
    assert_eq!(lifecycle.signer_epoch, 2);
    assert_eq!(lifecycle.block_hash, B256::repeat_byte(8));
    assert_eq!(lifecycle.approvals, [ApprovalStatus { approval, claimed: true }]);
    let accepted = chain.accepted_checkpoints(&at, &[cp.seq]).await.unwrap();
    accepted[0].verify(&cp).unwrap();
    assert_eq!(accepted[0].signer_epoch, 1);
    assert_eq!(chain.epoch_signers(&at, &[1, 2]).await.unwrap(), vec![at.state.signer; 2]);
    let calls = &server.state.lock().unwrap().calls;
    assert!(calls.len() >= 13);
    assert!(
        calls.iter().all(|(_, hash)| *hash == B256::repeat_byte(8)),
        "every eth_call uses finalized canonical hash"
    );
    for selector in [
        usedNoncesCall::SELECTOR,
        checkpointRootsCall::SELECTOR,
        checkpointSignerAtCall::SELECTOR,
        checkpointEpochAtCall::SELECTOR,
        signerAtEpochCall::SELECTOR,
    ] {
        assert!(calls.iter().any(|(actual, _)| *actual == hex::encode(selector)), "required pinned selector");
    }
}

#[tokio::test]
async fn signer_epoch_queries_bind_exact_calldata_and_refuse_future_missing_or_orphaned_history() {
    let dir = scratch_dir("payout-epoch-identity-rpc");
    let server = Server::start(&dir).await;
    let old = Address::repeat_byte(7);
    {
        let mut state = server.state.lock().unwrap();
        state.epoch = 2;
        state.epoch_signers.insert(1, old);
        let signer = state.signer;
        state.epoch_signers.insert(2, signer);
    }
    let chain = rpc(&server.url, &dir);
    let at = chain.observe(View::Finalized, &[]).await.unwrap();
    assert_eq!(chain.epoch_signers(&at, &[2, 1]).await.unwrap(), [at.state.signer, AccountId(old.into())]);
    assert!(chain.epoch_signers(&at, &[0]).await.is_err());
    assert!(chain.epoch_signers(&at, &[3]).await.is_err());
    assert!(chain.epoch_signers(&at, &[1; crate::checkpoints::APPROVAL_PAGE_SIZE + 1]).await.is_err());
    server.state.lock().unwrap().epoch_signers.remove(&1);
    assert!(chain.epoch_signers(&at, &[1]).await.is_err());
    server.state.lock().unwrap().orphan = Some(at.lifecycle.as_ref().unwrap().block_hash);
    assert!(chain.epoch_signers(&at, &[2]).await.is_err());
}

#[tokio::test]
async fn queried_paid_nonce_is_bound_to_both_account_and_nonce_in_real_calldata() {
    let dir = scratch_dir("payout-nonce-identity-rpc");
    let server = Server::start(&dir).await;
    let wanted = ApprovalId { account: AccountId([4; 20]), nonce: 7, approved_seq: 9 };
    {
        let mut state = server.state.lock().unwrap();
        state.claimed.insert((Address::from(wanted.account.0), wanted.nonce + 1));
        state.claimed.insert((Address::repeat_byte(5), wanted.nonce));
    }
    let at = rpc(&server.url, &dir).observe(View::Finalized, &[wanted]).await.unwrap();
    assert_eq!(at.lifecycle.unwrap().approvals, [ApprovalStatus { approval: wanted, claimed: false }]);
}

#[tokio::test]
async fn history_query_refuses_orphaned_hash_and_unaccepted_or_future_checkpoint() {
    let dir = scratch_dir("payout-history-orphan-rpc");
    let server = Server::start(&dir).await;
    server.state.lock().unwrap().finalized = Some(checkpoint(10));
    let chain = rpc(&server.url, &dir);
    let at = chain.observe(View::Finalized, &[]).await.unwrap();
    assert!(chain.accepted_checkpoints(&at, &[9]).await.unwrap_err().to_string().contains("not accepted"));
    assert!(chain.accepted_checkpoints(&at, &[11]).await.unwrap_err().to_string().contains("beyond"));
    server.state.lock().unwrap().orphan = Some(B256::repeat_byte(8));
    assert!(chain.accepted_checkpoints(&at, &[10]).await.unwrap_err().to_string().contains("canonical"));
}

#[tokio::test]
async fn retired_reservation_recovers_only_after_exact_finalized_history_authentication() {
    let dir = scratch_dir("payout-retired-reservation");
    fs::create_dir_all(&dir).unwrap();
    let old = checkpoint(10);
    let store =
        SubmissionStore::load(&dir, domain(), Address::repeat_byte(3), AccountId(EnclaveKey::test_key().address()))
            .unwrap();
    store.reserve(old.clone(), 7, None).unwrap();
    let before = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    let server = Server::start(&dir).await;
    let new = EnclaveKey::generate();
    {
        let mut state = server.state.lock().unwrap();
        state.signer = Address::from(new.address());
        state.epoch = 2;
        state.latest = Some(old.clone());
        state.finalized = Some(old.clone());
        state.accepted.insert(old.seq, (old.clone(), 1));
    }
    let chain = rotated_rpc(&server.url, &dir, AccountId(new.address()));
    assert!(chain.reserved_checkpoint().is_err(), "parsing retired bytes never grants authority");
    let at = chain.observe(View::Finalized, &[]).await.unwrap();
    chain.authenticate_reservation(&at).await.unwrap();
    assert_eq!(chain.reserved_checkpoint().unwrap(), Some(old));
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), before, "authentication is read only");
    assert!(server.state.lock().unwrap().sent.is_empty());
}

#[tokio::test]
async fn retired_reservation_wrong_root_or_unaccepted_identity_stays_unauthorized() {
    let dir = scratch_dir("payout-retired-forgery");
    fs::create_dir_all(&dir).unwrap();
    let old = checkpoint(10);
    let store =
        SubmissionStore::load(&dir, domain(), Address::repeat_byte(3), AccountId(EnclaveKey::test_key().address()))
            .unwrap();
    store.reserve(old.clone(), 7, None).unwrap();
    let before = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    let server = Server::start(&dir).await;
    let new = EnclaveKey::generate();
    {
        let mut state = server.state.lock().unwrap();
        state.signer = Address::from(new.address());
        state.epoch = 2;
        state.finalized = Some(checkpoint(11));
        let mut wrong = old.clone();
        wrong.root = format!("0x{}", "ff".repeat(32));
        state.accepted.insert(old.seq, (wrong, 1));
    }
    let chain = rotated_rpc(&server.url, &dir, AccountId(new.address()));
    let at = chain.observe(View::Finalized, &[]).await.unwrap();
    assert!(chain.authenticate_reservation(&at).await.is_err());
    assert!(chain.reserved_checkpoint().is_err());
    server.state.lock().unwrap().accepted.clear();
    assert!(chain.authenticate_reservation(&at).await.unwrap_err().to_string().contains("not accepted"));
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), before);
    assert!(server.state.lock().unwrap().sent.is_empty());
}

#[tokio::test]
async fn canonical_history_cannot_assign_a_future_signer_epoch() {
    let dir = scratch_dir("payout-future-epoch-rpc");
    let server = Server::start(&dir).await;
    let cp = checkpoint(10);
    {
        let mut state = server.state.lock().unwrap();
        state.finalized = Some(cp.clone());
        state.accepted.insert(cp.seq, (cp, 2));
    }
    let chain = rpc(&server.url, &dir);
    let at = chain.observe(View::Finalized, &[]).await.unwrap();
    assert!(chain.accepted_checkpoints(&at, &[10]).await.unwrap_err().to_string().contains("authority"));
}

#[tokio::test]
async fn latest_history_cannot_authorize_retired_signatures() {
    let dir = scratch_dir("payout-latest-history-rpc");
    let server = Server::start(&dir).await;
    server.state.lock().unwrap().latest = Some(checkpoint(10));
    let chain = rpc(&server.url, &dir);
    let at = chain.observe(View::Latest, &[]).await.unwrap();
    assert!(chain.accepted_checkpoints(&at, &[10]).await.unwrap_err().to_string().contains("finalized"));
}

#[tokio::test]
async fn oversized_approval_queries_fail_before_rpc_or_storage_access() {
    let dir = scratch_dir("payout-oversized-page-rpc");
    let chain = rpc("http://127.0.0.1:1", &dir);
    let approval = ApprovalId { account: AccountId([4; 20]), nonce: 7, approved_seq: 9 };
    let approvals = vec![approval; crate::checkpoints::APPROVAL_PAGE_SIZE + 1];
    assert!(chain.observe(View::Finalized, &approvals).await.unwrap_err().to_string().contains("page size"));
    assert!(!dir.exists());
}

#[tokio::test]
async fn read_only_audit_client_ignores_the_posting_journal_and_pins_all_history_reads() {
    let dir = scratch_dir("payout-readonly-audit-rpc");
    fs::create_dir_all(&dir).unwrap();
    let journal = dir.join("checkpoint-submission.json");
    fs::write(&journal, b"{malformed untrusted posting state").unwrap();
    let before = fs::read(&journal).unwrap();
    let server = Server::start(&dir).await;
    let cp = checkpoint(10);
    server.state.lock().unwrap().finalized = Some(cp.clone());
    let cfg: CheckpointSection = toml::from_str(&format!(
        "rpc_url = {:?}\nrelayer = '{:#x}'\nretry_interval = '10ms'\nrpc_timeout = '1s'",
        server.url,
        Address::repeat_byte(3),
    ))
    .unwrap();
    let chain = CheckpointRpc::read_only(&cfg, domain(), Address::repeat_byte(2), FinalityTag::Finalized, 0).unwrap();
    assert!(chain.post(&cp).await.unwrap_err().to_string().contains("read-only"));
    assert!(chain.reserved_checkpoint().unwrap_err().to_string().contains("read-only"));
    assert!(server.state.lock().unwrap().calls.is_empty(), "write refusal precedes even network reads");
    let at = chain.observe(View::Finalized, &[]).await.unwrap();
    chain.accepted_checkpoints(&at, &[10]).await.unwrap()[0].verify(&cp).unwrap();
    assert!(server.state.lock().unwrap().calls.iter().all(|(_, hash)| *hash == B256::repeat_byte(8)));
    assert!(server.state.lock().unwrap().sent.is_empty());
    assert_eq!(fs::read(&journal).unwrap(), before);
    assert!(!dir.join("checkpoint-submission.lock").exists());
    assert!(!dir.join("checkpoint-submission.json.tmp").exists());
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
}

#[tokio::test]
async fn new_signer_reuses_publicly_obsolete_reservation_with_the_same_nonce_fence() {
    let dir = scratch_dir("payout-rotated-nonce-reuse");
    fs::create_dir_all(&dir).unwrap();
    let old = checkpoint(10);
    let store =
        SubmissionStore::load(&dir, domain(), Address::repeat_byte(3), AccountId(EnclaveKey::test_key().address()))
            .unwrap();
    store.reserve(old.clone(), 7, None).unwrap();
    let server = Server::start(&dir).await;
    let new = EnclaveKey::generate();
    {
        let mut state = server.state.lock().unwrap();
        state.signer = Address::from(new.address());
        state.epoch = 2;
        state.latest = Some(old.clone());
        state.finalized = Some(old.clone());
        state.accepted.insert(old.seq, (old.clone(), 1));
    }
    let chain = rotated_rpc(&server.url, &dir, AccountId(new.address()));
    let at = chain.observe(View::Finalized, &[]).await.unwrap();
    chain.authenticate_reservation(&at).await.unwrap();
    let parent = CheckpointParent { has_checkpoint: true, seq: old.seq, root: old.root.parse().unwrap() };
    let successor = Checkpoint {
        seq: 11,
        root: format!("0x{}", "22".repeat(32)),
        signer: new.address_hex(),
        signature: new.sign_checkpoint_v2(domain().chain_id, &domain().vault.0, 11, &[0x22; 32], parent),
        parent: Some(parent),
        domain: Some(domain()),
        deferred_withdrawals: true,
    };
    chain.post(&successor).await.unwrap();
    let record = read_record(&dir);
    assert_eq!(record["nonce"], 7);
    assert_eq!(record["checkpoint"], serde_json::to_value(&successor).unwrap());
    assert_eq!(record["reused_through"], serde_json::to_value(&old).unwrap());
    assert_eq!(server.state.lock().unwrap().sent.len(), 1);
    assert_eq!(server.state.lock().unwrap().sent[0]["nonce"], "0x7");
    let bytes = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    server.state.lock().unwrap().latest_nonce = 9;
    assert!(chain.post(&successor).await.unwrap_err().to_string().contains("unrelated"));
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    assert_eq!(server.state.lock().unwrap().sent.len(), 1);
    assert!(chain.post(&old).await.is_err(), "accepted history does not reauthorize a retired key for a send");
}

// ── Helpers ──

fn rotated_rpc(url: &str, dir: &Path, signer: AccountId) -> CheckpointRpc {
    let cfg: CheckpointSection = toml::from_str(&format!(
        "rpc_url = {url:?}\nrelayer = '{:#x}'\nretry_interval = '10ms'\nrpc_timeout = '1s'",
        Address::repeat_byte(3),
    ))
    .unwrap();
    CheckpointRpc::new(&cfg, domain(), Address::repeat_byte(2), FinalityTag::Finalized, 0, dir, signer).unwrap()
}
