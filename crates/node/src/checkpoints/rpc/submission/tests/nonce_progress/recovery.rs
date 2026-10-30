use super::*;
use alloy::primitives::B256;

#[tokio::test]
async fn rejected_submission_public_finality_and_successor_reuse_survive_restart() {
    for restart in [false, true] {
        let dir = scratch_dir("nonce-progress-rejected");
        fs::create_dir_all(&dir).unwrap();
        let server = Server::start(&dir).await;
        let a = work(checkpoint(10));
        let b = work(successor(&a.checkpoint, 11));
        let mut worker = poster(&server, &dir);
        server.state.lock().unwrap().reject_send = true;
        assert!(worker.reconcile(&a).await.is_err());
        assert_eq!(read_record(&dir)["nonce"], 7);
        {
            let mut state = server.state.lock().unwrap();
            assert_eq!(state.pending_nonce, 7, "the signing RPC never accepted A");
            state.reject_send = false;
            state.latest = Some(a.checkpoint.clone());
        }
        assert_eq!(worker.reconcile(&a).await.unwrap(), (None, Health::Unvalidated));
        assert_eq!(server.state.lock().unwrap().sent.len(), 1, "latest A suppresses another send");
        server.state.lock().unwrap().finalized = Some(a.checkpoint.clone());
        assert_eq!(worker.reconcile(&a).await.unwrap(), (Some(10), Health::Healthy));
        if restart {
            worker = poster(&server, &dir);
        }
        assert!(worker.reconcile_known(&b, &[&a, &b]).await.is_err(), "B is sent but not included yet");
        let record = read_record(&dir);
        assert_eq!(record["checkpoint"], serde_json::to_value(&b.checkpoint).unwrap());
        assert_eq!(record["reused_through"], serde_json::to_value(&a.checkpoint).unwrap());
        assert_eq!(record["nonce"], 7);
        let sent = server.state.lock().unwrap().sent.clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1]["nonce"], "0x7");
        assert_ne!(sent[0]["input"], sent[1]["input"]);
        server.state.lock().unwrap().latest = Some(b.checkpoint.clone());
        worker.chain.post(&b.checkpoint).await.unwrap();
        assert_eq!(read_record(&dir), record, "latest inclusion is progress without journal retirement");
        assert_eq!(worker.reconcile_known(&b, &[&a, &b]).await.unwrap(), (Some(10), Health::Healthy));
        server.state.lock().unwrap().finalized = Some(b.checkpoint.clone());
        assert_eq!(worker.reconcile_known(&b, &[&a, &b]).await.unwrap(), (Some(11), Health::Healthy));
        assert_eq!(server.state.lock().unwrap().sent.len(), 2);
    }
}

#[tokio::test]
async fn resurfaced_obsolete_transaction_consuming_reused_nonce_recovers_exact_successor() {
    for lost_response in [false, true] {
        for restart in [false, true] {
            let dir = scratch_dir("nonce-progress-resurfaced");
            fs::create_dir_all(&dir).unwrap();
            let server = Server::start(&dir).await;
            let a = work(checkpoint(10));
            let b = work(successor(&a.checkpoint, 11));
            let mut worker = poster(&server, &dir);
            server.state.lock().unwrap().lose_response = lost_response;
            assert!(worker.reconcile(&a).await.is_err());
            assert_eq!(read_record(&dir)["transaction_hash"].is_null(), lost_response);
            public_finality_and_drop(&server, &a.checkpoint);
            assert_eq!(worker.reconcile(&a).await.unwrap(), (Some(10), Health::Healthy));
            if restart {
                worker = poster(&server, &dir);
            }
            assert!(worker.reconcile_known(&b, &[&a, &b]).await.is_err());
            let b_at_seven = server.state.lock().unwrap().sent[1].clone();
            assert_eq!(read_record(&dir)["nonce"], 7);
            // A returns to a miner after the journal has moved to B. Its obsolete call reverts,
            // consumes N, and leaves the actual finalized vault at B's signed predecessor A.
            {
                let mut state = server.state.lock().unwrap();
                state.latest_nonce = 8;
                state.pending_nonce = 8;
                state.consumer = Some(state.sent[0].clone());
                state.known = true;
            }
            if restart {
                worker = poster(&server, &dir);
            }
            assert!(worker.reconcile_known(&b, &[&a, &b]).await.is_err());
            let record = read_record(&dir);
            assert_eq!(record["checkpoint"], serde_json::to_value(&b.checkpoint).unwrap());
            assert_eq!(record["nonce"], 8);
            assert!(record.get("reused_through").is_none());
            let mut expected = b_at_seven;
            expected["nonce"] = json!("0x8");
            assert_eq!(server.state.lock().unwrap().sent[2], expected);
            worker = poster(&server, &dir);
            for _ in 0..5 {
                assert!(worker.reconcile_known(&b, &[&a, &b]).await.is_err());
            }
            assert_eq!(server.state.lock().unwrap().sent.len(), 3, "pending B never gets a third nonce");
            server.state.lock().unwrap().latest = Some(b.checkpoint.clone());
            assert_eq!(worker.reconcile_known(&b, &[&a, &b]).await.unwrap(), (Some(10), Health::Healthy));
            server.state.lock().unwrap().finalized = Some(b.checkpoint.clone());
            assert_eq!(worker.reconcile_known(&b, &[&a, &b]).await.unwrap(), (Some(11), Health::Healthy));
        }
    }
}

#[tokio::test]
async fn reverted_own_send_at_reused_nonce_recovers_to_the_next_nonce() {
    for restart in [false, true] {
        let dir = scratch_dir("nonce-progress-reverted-own-send");
        fs::create_dir_all(&dir).unwrap();
        let server = Server::start(&dir).await;
        let a = work(checkpoint(10));
        let b = work(successor(&a.checkpoint, 11));
        let mut chain = rpc(&server.url, &dir);
        chain.post(&a.checkpoint).await.unwrap();
        public_finality_and_drop(&server, &a.checkpoint);
        chain.post(&b.checkpoint).await.unwrap();
        let b_send = server.state.lock().unwrap().sent[1].clone();
        assert_eq!(read_record(&dir)["nonce"], 7);
        assert_eq!(read_record(&dir)["reused_through"], serde_json::to_value(&a.checkpoint).unwrap());
        // The guardian's retireSigner lands before B: B's own call reverts against the retired
        // signer, consuming its nonce while the vault never holds B.
        {
            let mut state = server.state.lock().unwrap();
            state.latest_nonce = 8;
            state.pending_nonce = 8;
            state.consumer = Some(b_send.clone());
        }
        if restart {
            chain = rpc(&server.url, &dir);
        }
        chain.post(&b.checkpoint).await.unwrap();
        let record = read_record(&dir);
        assert_eq!(record["nonce"], 8, "a reverted own send moves whatever the reuse provenance");
        assert!(record.get("reused_through").is_none());
        let sent = server.state.lock().unwrap().sent.clone();
        assert_eq!(sent.len(), 3);
        assert_eq!((sent[2]["nonce"].as_str(), &sent[2]["input"]), (Some("0x8"), &b_send["input"]));
        // The recovered reservation polls its known pending send without a third nonce.
        server.state.lock().unwrap().known = true;
        if restart {
            chain = rpc(&server.url, &dir);
        }
        for _ in 0..5 {
            chain.post(&b.checkpoint).await.unwrap();
        }
        assert_eq!(server.state.lock().unwrap().sent.len(), 3);
    }
}

#[tokio::test]
async fn repeated_public_supersession_retains_bounded_provenance_for_earliest_resurfaced_call() {
    let dir = scratch_dir("nonce-progress-repeated");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let a = checkpoint(10);
    let b = successor(&a, 11);
    let c = successor(&b, 12);
    let chain = rpc(&server.url, &dir);
    chain.post(&a).await.unwrap();
    public_finality_and_drop(&server, &a);
    chain.post(&b).await.unwrap();
    public_finality_and_drop(&server, &b);
    chain.post(&c).await.unwrap();
    assert_eq!(read_record(&dir)["reused_through"], serde_json::to_value(&b).unwrap());
    {
        let mut state = server.state.lock().unwrap();
        state.latest_nonce = 8;
        state.pending_nonce = 8;
        state.consumer = Some(state.sent[0].clone());
    }
    rpc(&server.url, &dir).post(&c).await.unwrap();
    assert_eq!(read_record(&dir)["nonce"], 8);
    assert_eq!(read_record(&dir)["checkpoint"], serde_json::to_value(c).unwrap());
    assert_eq!(server.state.lock().unwrap().sent.len(), 4);
}

#[tokio::test]
async fn obsolete_consumption_before_successor_acceptance_recovers_after_restart() {
    let dir = scratch_dir("nonce-progress-acceptance-race");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let a = work(checkpoint(10));
    let b = work(successor(&a.checkpoint, 11));
    let worker = poster(&server, &dir);
    assert!(worker.reconcile(&a).await.is_err());
    public_finality_and_drop(&server, &a.checkpoint);
    let bytes = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    fs::create_dir(dir.join("checkpoint-submission.json.tmp")).unwrap();
    assert!(worker.reconcile_known(&b, &[&a, &b]).await.is_err());
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    assert_eq!(server.state.lock().unwrap().sent.len(), 1, "reuse must be durable before sending B");
    fs::remove_dir(dir.join("checkpoint-submission.json.tmp")).unwrap();
    {
        let mut state = server.state.lock().unwrap();
        state.resurface_on_send = Some(state.sent[0].clone());
    }
    assert!(worker.reconcile_known(&b, &[&a, &b]).await.is_err());
    assert_eq!(read_record(&dir)["checkpoint"], serde_json::to_value(&b.checkpoint).unwrap());
    assert_eq!(read_record(&dir)["reused_through"], serde_json::to_value(&a.checkpoint).unwrap());
    assert_eq!(read_record(&dir)["transaction_hash"], Value::Null);
    let worker = poster(&server, &dir);
    assert!(worker.reconcile_known(&b, &[&a, &b]).await.is_err());
    assert_eq!(read_record(&dir)["nonce"], 8);
    assert_eq!(server.state.lock().unwrap().sent.len(), 3);
    server.state.lock().unwrap().latest = Some(b.checkpoint.clone());
    assert_eq!(worker.reconcile_known(&b, &[&a, &b]).await.unwrap(), (Some(10), Health::Healthy));
    server.state.lock().unwrap().finalized = Some(b.checkpoint.clone());
    assert_eq!(worker.reconcile_known(&b, &[&a, &b]).await.unwrap(), (Some(11), Health::Healthy));
}

#[tokio::test]
async fn public_finality_never_allocates_around_a_still_pending_predecessor() {
    let dir = scratch_dir("nonce-progress-pending");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let a = checkpoint(10);
    let b = successor(&a, 11);
    let chain = rpc(&server.url, &dir);
    chain.post(&a).await.unwrap();
    {
        let mut state = server.state.lock().unwrap();
        state.latest = Some(a.clone());
        state.finalized = Some(a.clone());
    }
    let bytes = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    for _ in 0..5 {
        assert!(rpc(&server.url, &dir).post(&b).await.unwrap_err().to_string().contains("pending"));
    }
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    assert_eq!(server.state.lock().unwrap().sent.len(), 1);
    public_finality_and_drop(&server, &a);
    rpc(&server.url, &dir).post(&b).await.unwrap();
    assert_eq!(read_record(&dir)["nonce"], 7);
    assert_eq!(server.state.lock().unwrap().sent.len(), 2);
}

#[tokio::test]
async fn unrelated_or_unauthorized_nonce_consumers_preserve_the_reservation() {
    for reuse in [false, true] {
        let dir = scratch_dir("nonce-progress-unrelated");
        fs::create_dir_all(&dir).unwrap();
        let server = Server::start(&dir).await;
        let a = checkpoint(10);
        let b = successor(&a, 11);
        let chain = rpc(&server.url, &dir);
        chain.post(&a).await.unwrap();
        public_finality_and_drop(&server, &a);
        if reuse {
            chain.post(&b).await.unwrap();
        }
        let bytes = fs::read(dir.join("checkpoint-submission.json")).unwrap();
        let transaction = server.state.lock().unwrap().sent[0].clone();
        let mut forged = a.clone();
        forged.root = format!("0x{}", "ff".repeat(32));
        let forged_input = crate::checkpoints::rpc::submission::posting::checkpoint_input(&forged).unwrap();
        let successor_input = crate::checkpoints::rpc::submission::posting::checkpoint_input(&b).unwrap();
        let mut other_parent = a.clone();
        other_parent.parent = successor(&checkpoint(9), 10).parent;
        other_parent.signature = EnclaveKey::test_key().sign_checkpoint_v2(
            domain().chain_id,
            &domain().vault.0,
            a.seq,
            &[0x11; 32],
            other_parent.parent.unwrap(),
        );
        let other_parent_input = crate::checkpoints::rpc::submission::posting::checkpoint_input(&other_parent).unwrap();
        for (field, value) in [
            ("to", json!(Address::repeat_byte(4))),
            ("from", json!(Address::repeat_byte(4))),
            ("chainId", json!("0x1")),
            ("value", json!("0x1")),
            ("input", json!("0x00")),
            ("input", json!(format!("0x{}", hex::encode(forged_input)))),
            ("input", json!(format!("0x{}", hex::encode(other_parent_input)))),
        ] {
            let mut consumer = transaction.clone();
            consumer[field] = value;
            {
                let mut state = server.state.lock().unwrap();
                state.latest_nonce = 8;
                state.pending_nonce = 8;
                state.consumer = Some(consumer);
            }
            assert!(chain.post(&b).await.is_err(), "{field}");
            assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes, "{field}");
        }
        // The reservation's own exact call is the one consumer that moves it: its send reverted
        // without the vault ever holding the checkpoint. A provenanceless reservation is `a`
        // itself, so the successor's call is not its own and still refuses.
        {
            let mut consumer = transaction.clone();
            consumer["input"] = json!(format!("0x{}", hex::encode(successor_input)));
            let mut state = server.state.lock().unwrap();
            state.latest_nonce = 8;
            state.pending_nonce = 8;
            state.consumer = Some(consumer);
        }
        if reuse {
            chain.post(&b).await.unwrap();
            assert_eq!(read_record(&dir)["nonce"], 8, "the reservation's own reverted call recovers it");
            assert_eq!(server.state.lock().unwrap().sent.len(), 3);
        } else {
            assert!(chain.post(&b).await.is_err(), "a successor's call is not the reservation's own");
            assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
            assert_eq!(server.state.lock().unwrap().sent.len(), 1);
        }
    }
}

#[tokio::test]
async fn recovery_errors_and_unfinalized_consumption_preserve_reuse_provenance() {
    let dir = scratch_dir("nonce-progress-errors");
    fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir).await;
    let a = checkpoint(10);
    let b = successor(&a, 11);
    let chain = rpc(&server.url, &dir);
    chain.post(&a).await.unwrap();
    public_finality_and_drop(&server, &a);
    chain.post(&b).await.unwrap();
    let bytes = fs::read(dir.join("checkpoint-submission.json")).unwrap();
    {
        let mut state = server.state.lock().unwrap();
        state.latest_nonce = 8;
        state.pending_nonce = 8;
        state.finalized = None;
    }
    assert!(chain.post(&b).await.is_err(), "reuse provenance cannot assert finalized coverage");
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    server.state.lock().unwrap().finalized = Some(a.clone());
    assert!(chain.post(&b).await.unwrap_err().to_string().contains("not finalized"));
    {
        let mut state = server.state.lock().unwrap();
        state.consumer = Some(state.sent[0].clone());
    }
    server.state.lock().unwrap().pending_nonce = 9;
    assert!(chain.post(&b).await.unwrap_err().to_string().contains("pending"));
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    server.state.lock().unwrap().pending_nonce = 8;
    server.state.lock().unwrap().latest_nonce = 9;
    assert!(chain.post(&b).await.unwrap_err().to_string().contains("unrelated"));
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    server.state.lock().unwrap().latest_nonce = 8;
    for hash in [8, 6, 5] {
        server.state.lock().unwrap().orphan = Some(B256::repeat_byte(hash));
        assert!(chain.post(&b).await.is_err());
        assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    }
    server.state.lock().unwrap().orphan = None;
    server.state.lock().unwrap().wrong_consuming_hash = true;
    assert!(chain.post(&b).await.is_err());
    server.state.lock().unwrap().wrong_consuming_hash = false;
    server.state.lock().unwrap().fail_method = Some("eth_getBlockByHash");
    assert!(chain.post(&b).await.is_err());
    server.state.lock().unwrap().fail_method = None;
    fs::create_dir(dir.join("checkpoint-submission.json.tmp")).unwrap();
    assert!(chain.post(&b).await.is_err());
    assert_eq!(fs::read(dir.join("checkpoint-submission.json")).unwrap(), bytes);
    assert_eq!(server.state.lock().unwrap().sent.len(), 2);
    fs::remove_dir(dir.join("checkpoint-submission.json.tmp")).unwrap();
    rpc(&server.url, &dir).post(&b).await.unwrap();
    assert_eq!(read_record(&dir)["nonce"], 8);
}
