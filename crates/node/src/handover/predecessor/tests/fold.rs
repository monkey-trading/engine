//! Keys move only under a generation the vault has folded: the predecessor refuses a successor whose
//! release change the generation has not folded, before it stops sequencing, and a successor whose
//! own read finds the change unfolded seals nothing.

use super::*;

/// A successor the vault names, but whose release change the generation has not folded, gets no
/// keys, and the predecessor goes on sequencing until the fold lands.
#[tokio::test]
async fn a_release_change_the_generation_has_not_folded_gets_no_keys_and_sequencing_goes_on() {
    let mut exchange = Exchange::start("handover-unfolded").await;
    exchange.make_upgrade_current();
    let running = Release { launch_digest: RELEASE, config_sha256: PREDECESSOR_CONFIG };
    *exchange.synced.lock().unwrap() = Some(running.id());
    let err = exchange.take().await.err().expect("the successor took keys under an unfolded generation");
    assert!(format!("{err:#}").contains("has not folded"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");
    assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "the successor sealed keys");

    // Once the generation folds the upgrade, the same successor takes the keys.
    *exchange.synced.lock().unwrap() = Some(exchange.upgrade().id());
    let head = write_log_tail(&exchange.successor_log, 3, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    exchange.take().await.unwrap();
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

/// The successor checks the fold itself before it seals: the host can hold its request while the
/// release moves, so the predecessor's admission is not enough.
#[tokio::test]
async fn a_successor_that_reads_an_unfolded_generation_seals_nothing() {
    let mut exchange = Exchange::start("handover-unfolded-successor").await;
    exchange.make_upgrade_current();
    let head = write_log_tail(&exchange.successor_log, 3, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    // The predecessor's read finds the upgrade folded; the successor's own read does not.
    *exchange.synced.lock().unwrap() = Some(exchange.upgrade().id());
    let signer = AccountId(exchange.old.enclave_key().unwrap().address());
    let unfolded = Vault {
        signer,
        current: exchange.current.clone(),
        synced: Arc::new(Mutex::new(Some([0x99; 32]))),
        rotation: Default::default(),
    };
    let err = exchange.take_reading(unfolded).await.err().expect("the successor sealed under an unfolded generation");
    assert!(format!("{err:#}").contains("has not folded"), "{err:#}");
    assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "the successor sealed keys");

    // The stopped predecessor keeps serving, and a successor that reads the fold takes the keys.
    exchange.take().await.unwrap();
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}
