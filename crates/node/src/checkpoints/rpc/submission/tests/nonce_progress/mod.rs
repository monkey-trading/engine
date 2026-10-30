//! Integrated public-relay and obsolete-nonce recovery through Poster and the real RPC adapter.

use super::*;

mod recovery;

#[tokio::test]
async fn signer_parent_and_finality_mismatches_never_replace_the_reservation() {
    let dir = scratch_dir("nonce-progress-authority");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let a = checkpoint(10);
    let b = successor(&a, 11);
    let chain = rpc(&server.url, &dir);
    chain.post(&a).await.unwrap();
    public_finality_and_drop(&server, &a);
    let bytes = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    server.state.lock().unwrap().finalized = None;
    assert!(chain.post(&b).await.is_err());
    server.state.lock().unwrap().finalized = Some(a.clone());
    server.state.lock().unwrap().finalized_signer = Some(Address::repeat_byte(9));
    assert!(chain.post(&b).await.is_err());
    server.state.lock().unwrap().finalized_signer = None;
    server.state.lock().unwrap().signer = Address::repeat_byte(9);
    assert!(chain.post(&b).await.is_err());
    server.state.lock().unwrap().signer = Address::from(EnclaveKey::test_key().address());
    let other_parent = checkpoint(9);
    let wrong_parent = successor(&other_parent, 11);
    assert!(chain.post(&wrong_parent).await.is_err());
    server.state.lock().unwrap().latest = Some(other_parent);
    assert!(chain.post(&b).await.is_err());
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    assert_eq!(server.state.lock().unwrap().sent.len(), 1);
}

#[tokio::test]
async fn consumed_nonce_must_reach_the_configured_confirmation_ancestor() {
    let dir = scratch_dir("nonce-progress-confirmations");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let a = checkpoint(10);
    let b = successor(&a, 11);
    let chain = rpc_with_finality(&server.url, &dir, FinalityTag::Latest, 2);
    chain.post(&a).await.unwrap();
    public_finality_and_drop(&server, &a);
    chain.post(&b).await.unwrap();
    let bytes = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    {
        let mut state = server.state.lock().unwrap();
        state.latest_nonce = 8;
        state.pending_nonce = 8;
        state.consumer = Some(state.sent[0].clone());
        state.consumed_at = 9;
    }
    assert!(chain.post(&b).await.unwrap_err().to_string().contains("not finalized"));
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    assert_eq!(server.state.lock().unwrap().sent.len(), 2);
    server.state.lock().unwrap().consumed_at = 6;
    chain.post(&b).await.unwrap();
    assert_eq!(read_record(&dir)["nonce"], 8);
    assert_eq!(server.state.lock().unwrap().sent.len(), 3);
}

#[test]
fn existing_records_load_and_invalid_reuse_provenance_preserves_bytes() {
    let dir = scratch_dir("nonce-progress-records");
    fs::create_dir_all(&dir).unwrap();
    let signer = AccountId(EnclaveKey::test_key().address());
    let store = SubmissionStore::load(&dir, domain(), Address::repeat_byte(3), signer).unwrap();
    let a = checkpoint(10);
    let b = successor(&a, 11);
    store.reserve(a.clone(), 7, None).unwrap();
    let legacy = read_record(&dir);
    assert!(legacy.get("reused_through").is_none(), "original v1 records have no provenance field");
    assert_eq!(store.read().unwrap().unwrap().checkpoint, a);
    store.reserve(b.clone(), 7, Some(a.clone())).unwrap();
    let valid = read_record(&dir);
    let mut cases = Vec::new();
    for field in ["root", "signer"] {
        let mut forged = valid.clone();
        forged["reused_through"][field] = json!("0x00");
        cases.push(forged);
    }
    let mut foreign = valid.clone();
    foreign["reused_through"]["domain"]["chain_id"] = json!(1);
    cases.push(foreign);
    let mut future = valid.clone();
    future["reused_through"] = serde_json::to_value(&b).unwrap();
    cases.push(future);
    // A separately valid checkpoint at the parent's sequence must also match its signed root.
    let key = EnclaveKey::test_key();
    let mut other = a.clone();
    other.root = format!("0x{}", "44".repeat(32));
    other.signature = key.sign_checkpoint_v2(domain().chain_id, &domain().vault.0, 10, &[0x44; 32], a.parent.unwrap());
    let mut conflicting = valid.clone();
    conflicting["reused_through"] = serde_json::to_value(other).unwrap();
    cases.push(conflicting);
    for invalid in cases {
        let bytes = serde_json::to_vec(&invalid).unwrap();
        fs::write(dir.join("checkpoint-submission.json"), &bytes).unwrap();
        assert!(SubmissionStore::load(&dir, domain(), Address::repeat_byte(3), signer).is_err());
        assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }
}

// ── Helpers ──

/// Signs a successor whose exact parent is independently authenticated by the fixture.
pub(super) fn successor(previous: &Checkpoint, seq: u64) -> Checkpoint {
    let key = EnclaveKey::test_key();
    let parent = CheckpointParent { has_checkpoint: true, seq: previous.seq, root: previous.root.parse().unwrap() };
    let root = [seq as u8; 32];
    Checkpoint {
        parent: Some(parent),
        seq,
        root: format!("0x{}", hex::encode(root)),
        signature: key.sign_checkpoint_v2(domain().chain_id, &domain().vault.0, seq, &root, parent),
        signer: key.address_hex(),
        domain: Some(domain()),
        deferred_withdrawals: true,
    }
}

/// Builds a fresh worker over persisted exact RPC submission state.
fn poster(server: &Server, dir: &Path) -> Poster<CheckpointRpc> {
    Poster {
        chain: rpc(&server.url, dir),
        registry: None,
        domain: domain(),
        signer: AccountId(EnclaveKey::test_key().address()),
        retry: Duration::from_millis(10),
        pending_deposits_critical_after: Duration::from_secs(1),
        post_withheld: Default::default(),
    }
}

/// Supplies exact authenticated work with the same zero collateral as the fixture vault.
fn work(checkpoint: Checkpoint) -> Work {
    Work {
        checkpoint,
        liabilities: Fixed::ZERO,
        deposits: 0,
        authorized: Fixed::ZERO,
        coverage: crate::checkpoints::DepositCoverage::Historical,
        distributed: engine_core::UsdcUnits(0),
    }
}

/// Models permissionless finality followed by the relayer transaction leaving every mempool.
fn public_finality_and_drop(server: &Server, checkpoint: &Checkpoint) {
    let mut state = server.state.lock().unwrap();
    state.latest = Some(checkpoint.clone());
    state.finalized = Some(checkpoint.clone());
    // A finalized checkpoint stays in the vault's accepted history after its successors.
    state.accepted.insert(checkpoint.seq, (checkpoint.clone(), 1));
    state.pending_nonce = state.latest_nonce;
    state.known = false;
}
