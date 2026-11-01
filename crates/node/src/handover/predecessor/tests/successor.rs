//! The successor's side over loopback: the final log segment rides with the keys into the
//! successor's log, a segment that differs from its listing is refused before the confirmation goes
//! out, and without the signed acknowledgement the keys stay sealed but the start fails.

use super::*;
/// The final segment rides with the keys: the predecessor's sealed tail lands in the successor's
/// log, and the successor confirms only because the log then ends at the handed-over head.
#[tokio::test]
async fn the_final_segment_rides_with_the_keys_into_an_empty_log() {
    let mut exchange = Exchange::start("handover-segment").await;
    exchange.make_upgrade_current();
    let head = Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) };
    // The predecessor's drained log lists the whole log; the successor's copy holds nothing yet,
    // as a cold start the archive backfilled nothing into, so the handover carries it from seq 0.
    write_log_from(&exchange.predecessor_log, &head, 0);
    let answer = exchange.answer_drain(Ok(head.clone()));
    let store = exchange.take().await.unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
    assert_eq!(store.keys().predecessor_head, Some(head.clone()));
    // The segment the predecessor sealed is in the successor's log, manifest and all, and the log
    // ends at the head.
    let log = crate::log::SegmentedLog::open(&exchange.successor_log, |_| Ok(())).unwrap();
    assert_eq!(log.last_seq(), Some(head.seq));
    assert_eq!(log.last_listed_segment().map(|listed| listed.last_seq), Some(head.seq));
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
}

/// A segment whose bytes differ from the listing inside the quote-bound message is refused before
/// the confirmation goes out: the successor does not start on a log the host tampered with.
#[tokio::test]
async fn segment_bytes_that_differ_from_the_listing_inside_the_keys_are_refused_before_confirming() {
    let (successor, _successor_dir) = successor_config("handover-segment-lie");
    let head = Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) };
    let old = KeySet::generate().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (domain, config) = (domain_of(&successor), successor.file_sha256);
    let mut handed = old.clone();
    handed.predecessor_head = Some(head.clone());
    let predecessor = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let challenge = [0x5c; 32];
        send(&mut stream, &Challenge { nonce: hex::encode(challenge) }).await.unwrap();
        let request: Request = receive(&mut stream).await.unwrap();
        let public_key = hex::decode(&request.public_key).unwrap();
        let report_data = request_report_data(domain, &challenge, &public_key, &config);
        let platform = Platform { launch_digest: RELEASE, policy: 0, tcb: 0 };
        let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
        // The real segment, whose listing travels inside the keys, attested; the host swaps the
        // wire bytes for others after the predecessor sealed them.
        let text = serde_json::to_string(&LogEntry::Command(command(1))).unwrap() + "\n";
        let bytes = text.as_bytes();
        let listed = crate::log::SegmentInfo {
            file: format!("{:020}.jsonl", 1),
            first_seq: 1,
            last_seq: 1,
            bytes: bytes.len() as u64,
            sha256: hex::encode(sha2::Sha256::digest(bytes)),
        };
        handed.handover_tails = vec![listed];
        let (encapped_key, ciphertext) = seal_keys(domain, &public_key, &report_data, &handed).unwrap();
        let bound = keys_report_data(domain, &report_data, &encapped_key, &ciphertext);
        let evidence = evidence::produce(&quoting, &MockAttestor, &platform, bound).await.unwrap();
        let keys =
            Reply::Keys { encapped_key: hex::encode(encapped_key), ciphertext: hex::encode(ciphertext), evidence };
        send(&mut stream, &keys).await.unwrap();
        send_bytes(&mut stream, b"lies").await.unwrap();
    });
    let named = CurrentRelease { id: Release { launch_digest: UPGRADE, config_sha256: config }.id(), generation: 4 };
    let vault = Vault { signer: AccountId(old.enclave_key().unwrap().address()), current: Arc::new(Mutex::new(named)) };
    let sealer = MockSealer { launch_digest: UPGRADE };
    let err =
        take(&successor, &address, &sealer, &MockAttestor, &vault).await.err().expect("started on a tampered log");
    assert!(format!("{err:#}").contains("differs from its listing"), "{err:#}");
    predecessor.await.unwrap();
}

#[tokio::test]
async fn a_successor_without_the_signed_acknowledgement_keeps_the_keys_sealed_but_does_not_start() {
    let (successor, successor_dir) = successor_config("handover-forged-ack");
    let head = Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) };
    write_log_tail(&successor.node.log_dir, &head);
    let old = KeySet::generate().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (domain, config) = (domain_of(&successor), successor.file_sha256);
    let mut handed = old.clone();
    handed.predecessor_head = Some(Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) });
    // A predecessor whose acknowledgement the host forges with a key of its own.
    let predecessor = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let challenge = [0x5c; 32];
        send(&mut stream, &Challenge { nonce: hex::encode(challenge) }).await.unwrap();
        let request: Request = receive(&mut stream).await.unwrap();
        let public_key = hex::decode(&request.public_key).unwrap();
        let report_data = request_report_data(domain, &challenge, &public_key, &config);
        let (encapped_key, ciphertext) = seal_keys(domain, &public_key, &report_data, &handed).unwrap();
        let bound = keys_report_data(domain, &report_data, &encapped_key, &ciphertext);
        let platform = Platform { launch_digest: RELEASE, policy: 0, tcb: 0 };
        let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
        let evidence = evidence::produce(&quoting, &MockAttestor, &platform, bound).await.unwrap();
        let keys =
            Reply::Keys { encapped_key: hex::encode(encapped_key), ciphertext: hex::encode(ciphertext), evidence };
        send(&mut stream, &keys).await.unwrap();
        let _: Confirm = receive(&mut stream).await.unwrap();
        let forged = EnclaveKey::generate().sign_digest(confirmed_digest(domain, &report_data));
        send(&mut stream, &Reply::Confirmed { signature: forged }).await.unwrap();
    });
    let named = CurrentRelease { id: Release { launch_digest: UPGRADE, config_sha256: config }.id(), generation: 2 };
    let vault = Vault { signer: AccountId(old.enclave_key().unwrap().address()), current: Arc::new(Mutex::new(named)) };
    let sealer = MockSealer { launch_digest: UPGRADE };
    let err = take(&successor, &address, &sealer, &MockAttestor, &vault).await.err().expect("started unacknowledged");
    assert!(format!("{err:#}").contains("did not acknowledge"), "{err:#}");
    predecessor.await.unwrap();

    // The keys stay sealed here, so a later start opens them without a flag.
    let reopened = KeyStore::open(&successor_dir, &sealer, config, 2).unwrap().expect("the keys were not sealed");
    assert_eq!(reopened.keys().enclave_key().unwrap().address(), old.enclave_key().unwrap().address());
}

#[test]
fn a_successor_runs_under_the_same_policy_and_no_lower_tcb() {
    let packed = |snp: u64| (8u64 << 56) | (snp << 48) | 3;
    let own = Platform { launch_digest: RELEASE, policy: 0x30000, tcb: packed(8) };
    let peer = |policy, snp| Peer { launch_digest: UPGRADE, policy, tcb: Some(Tcb::from_packed(packed(snp))) };
    no_weaker(&own, &peer(0x30000, 8)).unwrap();
    no_weaker(&own, &peer(0x30000, 9)).unwrap();
    let err = no_weaker(&own, &peer(0x30000 | 1 << 19, 8)).unwrap_err();
    assert!(err.to_string().contains("guest policy"), "a debug policy passed: {err:#}");
    let err = no_weaker(&own, &peer(0x30000, 7)).unwrap_err();
    assert!(err.to_string().contains("below this platform's committed"), "{err:#}");
    // Mock peers carry no TCB.
    no_weaker(
        &Platform { launch_digest: RELEASE, policy: 0, tcb: 0 },
        &Peer { launch_digest: UPGRADE, policy: 0, tcb: None },
    )
    .unwrap();
}
