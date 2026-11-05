//! The first finalized block a relay accepts.

use super::*;

#[tokio::test]
async fn a_turn_is_relayed_from_the_finalized_block_at_the_first_slot_after_its_seed_epoch() {
    // Turn T with seed epoch e is first ready with a header at slot (e + 1) * 32, the finalized
    // checkpoint block of the next epoch: the first slot the relay takes.
    let first = (seed_epoch(FINAL + 1, GENESIS).unwrap() + 1) * SLOTS_PER_EPOCH;
    let fake = Fake::new(first);
    relayer(&fake).poll().await;
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let call = ISeedRelay::relayCall::abi_decode(&sent[0].0).unwrap();
    assert_eq!((call.turn, call.witness.header.slot), (FINAL + 1, first));
}
