//! Which nonce each relay goes out at, and when the node stops tracking the one in flight.

use super::*;

const HOUR: Duration = Duration::from_secs(3601);

#[tokio::test(start_paused = true)]
async fn a_replacement_after_a_lost_answer_goes_out_at_the_nonce_of_the_relay_it_replaces() {
    let fake = Fake::new(slot_past(FINAL + 2));
    *fake.nonces.lock().unwrap() = (7, 7);
    let mut relayer = relayer(&fake);
    relayer.poll().await;
    // Relay A waits at nonce 7. Its replacement B is accepted at 7, but the answer is lost, so the node
    // still tracks A though B displaced it from the pool.
    *fake.nonces.lock().unwrap() = (7, 8);
    *fake.lose_answer.lock().unwrap() = true;
    tokio::time::advance(HOUR).await;
    relayer.poll().await;
    // At the next poll the node replaces A again, and the endpoint no longer finds A by hash: the
    // send still names nonce 7, so it does not queue behind B at 8.
    tokio::time::advance(HOUR).await;
    relayer.poll().await;
    let hashes = fake.hashes.lock().unwrap().clone();
    let sent = fake.sent.lock().unwrap().clone();
    let slots: Vec<_> = sent.iter().map(|relay| (relay.3, relay.2)).collect();
    assert_eq!(slots, [(7, None), (7, Some(hashes[0])), (7, Some(hashes[0]))]);
}

#[tokio::test(start_paused = true)]
async fn a_relay_whose_nonce_the_relayer_has_passed_without_its_receipt_is_no_longer_tracked() {
    let (_guard, mut alerts, _) = capture_alerts_and_resolutions();
    let fake = Fake::new(slot_past(FINAL + 2));
    *fake.nonces.lock().unwrap() = (7, 7);
    let mut relayer = relayer(&fake);
    relayer.poll().await;
    // Another transaction took nonce 7 while a transaction of the node's is still pending at 8: the
    // node stops tracking the relay and, as for any pending transaction it does not track, waits.
    *fake.nonces.lock().unwrap() = (8, 9);
    tokio::time::advance(HOUR).await;
    relayer.poll().await;
    assert_eq!(fake.sent.lock().unwrap().len(), 1, "no replacement of a nonce that is spent");
    assert!(relayer.in_flight.is_none());
    assert!(named(&mut alerts, "seed_relay_failed").iter().all(|a| !a["message"].contains("replacing it")));
    // With nothing pending, the next relay goes out fresh at the relayer's latest nonce.
    *fake.nonces.lock().unwrap() = (9, 9);
    relayer.poll().await;
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!((sent[1].3, sent[1].2), (9, None));
}
