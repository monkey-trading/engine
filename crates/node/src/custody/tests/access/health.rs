//! Source health from pending receipts, across a restart.

use super::*;

/// What replaying the node's log gives: the pending receipts and whether a source failure
/// episode is still open.
fn replayed(dir: &Path) -> (usize, bool) {
    let replayed = crate::sequencer::replay(&log_entries(&dir.join("log"))).unwrap();
    (replayed.engine.pending_deposits().len(), replayed.access_failed)
}

#[tokio::test]
async fn recovery_reopens_the_source_alert_only_for_receipts_pending_on_a_source_failure() {
    let dir = scratch("access-health-recovery");
    let (config, clear, _calls, source_task) = source(outage_answer).await;
    clear.store(true, Ordering::SeqCst);
    let client = Arc::new(AccessClient::new(&config).unwrap());
    let node = Node::start_with_policy(&dir, Some(config.policy()));
    activate(&node, config.policy()).await;
    let trader = eip712::address(client_key().verifying_key());
    let chains = chains(10);
    add(&chains, &funding(0, Beneficiary::Trader(trader), trader));
    let mut watch = screened_watcher(&chains, &dir, &client);
    assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
    node.stop().await;
    assert_eq!(replayed(&dir), (1, false), "waiting for a declaration says nothing about the source");

    let node = Node::start_with_policy(&dir, Some(config.policy()));
    attest(&node, trader).await;
    clear.store(false, Ordering::SeqCst);
    add(&chains, &funding(1, Beneficiary::Trader(trader), trader));
    let mut watch = screened_watcher(&chains, &dir, &client);
    assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
    node.stop().await;
    assert_eq!(replayed(&dir), (2, true), "a receipt pending on an unavailable source leaves the episode open");
    source_task.abort();
    let _ = source_task.await;
}
