//! What a promoted member's restart finds of its promotion: it resumes only while the same promotion
//! stands, the vault names its key in the promotion's epoch, and the registry names its release in the
//! promotion's generation.

use super::*;
use crate::liveness::Gate;
use crate::node::member::promoted::watch::{Settled, Watched, settled, watch};
use crate::node::member::promoted::{Record, Standing};

#[tokio::test]
async fn a_restart_resumes_only_while_the_registry_names_its_release_in_the_promotions_generation() {
    let key = AccountId([0x5b; 20]);
    let own = B256::from(release().id());
    let record = Record::new(CANDIDATE, key, PROMOTION, 121);
    let standing = |change: fn(&mut ChainState)| async move {
        let chain = FakeChain::new();
        chain.register(CANDIDATE, release().id(), true);
        chain.promote(CANDIDATE, key);
        chain.set(change);
        record.standing(&chain, &DOMAIN, own).await.unwrap()
    };
    assert_eq!(standing(|_| {}).await, Standing::Stands);
    type Change = fn(&mut ChainState);
    let ended: [(&str, Change); 4] = [
        ("a later generation of the same release", |chain| chain.rotation.1 += 1),
        ("another release in the same generation", |chain| chain.rotation.0 = B256::repeat_byte(0xee)),
        ("the vault left the promotion's epoch", |chain| chain.epoch += 1),
        ("the guardian retired the key", |chain| chain.signer = Address::ZERO),
    ];
    for (case, change) in ended {
        assert_eq!(standing(change).await, Standing::Ended, "{case}");
    }
    // The same changes before a final block records the promotion keep its record.
    let ended_unfinal: [(&str, Change); 4] = [
        ("a later generation of the same release", |chain| {
            (chain.rotation.1, chain.unfinal) = (chain.rotation.1 + 1, true)
        }),
        ("another release in the same generation", |chain| {
            (chain.rotation.0, chain.unfinal) = (B256::repeat_byte(0xee), true);
        }),
        ("the vault left the promotion's epoch", |chain| (chain.epoch, chain.unfinal) = (chain.epoch + 1, true)),
        ("the guardian retired the key", |chain| (chain.signer, chain.unfinal) = (Address::ZERO, true)),
    ];
    for (case, change) in ended_unfinal {
        assert_eq!(standing(change).await, Standing::Passed, "{case}, not final yet");
    }
    let removed = |chain: &mut ChainState| chain.assignments.clear();
    assert_eq!(standing(removed).await, Standing::Gone, "a final block past the promotion lacks it");
    let unfinal = |chain: &mut ChainState| chain.unfinal = true;
    assert_eq!(standing(unfinal).await, Standing::Stands, "no reorg seen: the latest block's record stands");
}

/// Once the promotion's watch saw a latest block lacking it, a later block recording it again resumes
/// nothing until a final block records it too.
#[tokio::test]
async fn a_promotion_a_reorg_took_resumes_only_once_a_final_block_records_it_again() {
    let key = AccountId([0x5b; 20]);
    let record = Record { reorged: true, ..Record::new(CANDIDATE, key, PROMOTION, 121) };
    let chain = FakeChain::new();
    chain.register(CANDIDATE, release().id(), true);
    chain.promote(CANDIDATE, key);
    chain.set(|chain| chain.unfinal = true);
    let own = B256::from(release().id());
    assert_eq!(record.standing(&chain, &DOMAIN, own).await.unwrap(), Standing::Unsettled);
    chain.set(|chain| chain.unfinal = false);
    assert_eq!(record.standing(&chain, &DOMAIN, own).await.unwrap(), Standing::Stands);
}

/// A rebuild reads the key of every signer epoch from the finalized one to the latest block's: an
/// intermediate promotion's, not final yet, sealed commands the copy holds.
#[tokio::test]
async fn a_rebuild_reads_every_key_the_latest_block_names_past_finality() {
    let (b, c, d) = (Address::repeat_byte(0xb0), Address::repeat_byte(0xc0), Address::repeat_byte(0xd0));
    let chain = FakeChain::new();
    chain.set(|chain| {
        chain.epoch = 4;
        chain.history = BTreeMap::from([(2, b), (3, c), (4, d)]);
    });
    let block = chain.latest().await.unwrap();
    let named: Vec<Address> = signers_from(&block, VAULT, 3).await.unwrap().into_iter().map(address_of).collect();
    assert_eq!(named, vec![c, d], "from the finalized epoch to the latest one");
    assert_eq!(signers_from(&block, VAULT, 5).await.unwrap(), Vec::new(), "no epoch past the latest");
}

/// `account` as an address.
fn address_of(account: AccountId) -> Address {
    Address::from(account.0)
}

/// Finality passes a promotion only at or past the block the start read it at, whatever the blocks'
/// times: a final block sharing the promotion's second but below that block settles nothing. A latest
/// block below it, as a lagging provider majority serves, says nothing of the promotion.
#[tokio::test]
async fn a_restart_judges_the_promotion_by_the_block_it_read_it_at_not_by_time() {
    let key = AccountId([0x5b; 20]);
    let own = B256::from(release().id());
    let chain = FakeChain::new();
    chain.register(CANDIDATE, release().id(), true);
    chain.promote(CANDIDATE, key);
    let now = chain.0.lock().unwrap().time;
    let read_at = Promotion { at: now, block: now, ..PROMOTION };
    let record = Record::new(CANDIDATE, key, read_at, 121);
    chain.set(|chain| {
        chain.assignments.clear();
        chain.final_lag = 1;
    });
    assert_eq!(record.standing(&chain, &DOMAIN, own).await.unwrap(), Standing::Unsettled, "finality is behind it");
    chain.set(|chain| chain.final_lag = 0);
    assert_eq!(record.standing(&chain, &DOMAIN, own).await.unwrap(), Standing::Gone, "a final block at it lacks it");
    let ahead = Record { block: now + 1, ..record };
    assert_eq!(ahead.standing(&chain, &DOMAIN, own).await.unwrap(), Standing::Behind, "the latest block is older");
    // A later signer epoch ends the promotion's term only once a final block at or past it records it.
    chain.promote(CANDIDATE, key);
    chain.set(|chain| (chain.epoch, chain.final_lag) = (chain.epoch + 1, 1));
    assert_eq!(record.standing(&chain, &DOMAIN, own).await.unwrap(), Standing::Passed, "finality is behind it");
    chain.set(|chain| chain.final_lag = 0);
    assert_eq!(record.standing(&chain, &DOMAIN, own).await.unwrap(), Standing::Ended, "a final block at it has it");
}

/// A restart that resumes from the promotion's record finds the turn in effect its generation is carried
/// into, as a start from the standby seal does, so its first command there is its `TurnTaken`.
#[tokio::test]
async fn a_resumed_promotion_names_the_drawn_turn_it_is_carried_into() {
    let key = AccountId([0x5b; 20]);
    let chain = FakeChain::new();
    chain.register(CANDIDATE, release().id(), true);
    chain.promote(CANDIDATE, key);
    let record = Record::new(CANDIDATE, key, PROMOTION, 121);
    assert_eq!(record.carried(&chain, &DOMAIN).await.unwrap(), PROMOTION.turn, "its own turn while it is the latest");
    chain.draw_next(CANDIDATE, PROMOTION.generation);
    assert_eq!(record.carried(&chain, &DOMAIN).await.unwrap(), PROMOTION.turn + 1, "the drawn turn it is carried into");
    chain.draw_next(AccountId([0x77; 20]), PROMOTION.generation);
    assert_eq!(record.carried(&chain, &DOMAIN).await.unwrap(), PROMOTION.turn, "another member's turn carries nothing");
    // A promotion carried through more drawn turns than a start's walk follows still resumes.
    let chain = FakeChain::new();
    chain.promote(CANDIDATE, key);
    (0..70).for_each(|_| chain.draw_next(CANDIDATE, PROMOTION.generation));
    assert_eq!(record.carried(&chain, &DOMAIN).await.unwrap(), PROMOTION.turn + 70);
}

/// The promotion's watch: a latest block recording the promotion stands until a final one does, a
/// latest block older than the one the start read it at stands too, and one at or past it that lacks
/// it is missing.
#[tokio::test]
async fn the_watch_reads_a_missing_promotion_only_from_a_block_at_or_past_the_starts() {
    let key = AccountId([0x5b; 20]);
    let chain = FakeChain::new();
    chain.register(CANDIDATE, release().id(), true);
    chain.promote(CANDIDATE, key);
    let now = chain.0.lock().unwrap().time;
    let promotion = Promotion { block: now, ..PROMOTION };
    assert_eq!(settled(&chain, &DOMAIN, key, &promotion).await.unwrap(), Settled::Final);
    chain.set(|chain| chain.unfinal = true);
    assert_eq!(settled(&chain, &DOMAIN, key, &promotion).await.unwrap(), Settled::Standing);
    chain.set(|chain| chain.assignments.clear());
    assert_eq!(settled(&chain, &DOMAIN, key, &promotion).await.unwrap(), Settled::Missing);
    let later = Promotion { block: now + 1, ..promotion };
    assert_eq!(settled(&chain, &DOMAIN, key, &later).await.unwrap(), Settled::Standing, "an older latest block");
}

/// Once a latest block lacks the promotion, the watch stops the gate, marks the reorg in the record,
/// so no restart resumes it before a final block records it again, and ends the node's run.
#[tokio::test(start_paused = true)]
async fn a_missing_promotion_stops_signing_and_marks_the_reorg_before_the_watch_ends() {
    let key = AccountId([0x5b; 20]);
    let chain = FakeChain::new();
    chain.register(CANDIDATE, release().id(), true);
    chain.promote(CANDIDATE, key);
    chain.set(|chain| chain.unfinal = true);
    let sealing = scratch_dir("promotion-watch");
    let promotion = Promotion { block: chain.0.lock().unwrap().time, ..PROMOTION };
    Record::new(CANDIDATE, key, promotion, 121).write_for_test(&sealing);
    let (gate, watched) = (Gate::default(), Watched { domain: DOMAIN, key, promotion, from_seq: 121 });
    let watching = watch(&chain, &watched, &gate, Some(&sealing));
    tokio::pin!(watching);
    let standing = tokio::time::timeout(std::time::Duration::from_secs(30), &mut watching).await;
    assert!(standing.is_err() && gate.signs(), "the watch ended while the promotion stood");
    chain.set(|chain| chain.assignments.clear());
    let ended = tokio::time::timeout(std::time::Duration::from_secs(30), watching).await.expect("the watch ended");
    assert!(ended.is_err(), "a missing promotion ends the run");
    assert!(!gate.signs(), "signing stopped");
    assert!(Record::load(&sealing).unwrap().is_some_and(|record| record.reorged), "the reorg is recorded");
}

/// A restart that finds the latest block lacking the promotion records the reorg before it waits, so a
/// later restart that finds the promotion again at a latest block, not final yet, still waits for a
/// final block to record it.
#[tokio::test]
async fn a_restart_that_sees_the_promotion_missing_records_the_reorg_before_it_waits() {
    let key = AccountId([0x5b; 20]);
    let own = B256::from(release().id());
    let chain = FakeChain::new();
    chain.register(CANDIDATE, release().id(), true);
    chain.promote(CANDIDATE, key);
    let sealing = scratch_dir("promotion-restart-reorg");
    let promotion = Promotion { block: chain.0.lock().unwrap().time, ..PROMOTION };
    let record = Record::new(CANDIDATE, key, promotion, 121);
    record.write_for_test(&sealing);
    let assignments = chain.0.lock().unwrap().assignments.clone();
    chain.set(|chain| {
        chain.assignments.clear();
        chain.final_lag = 1;
    });
    let standing = record.standing_at_start(&chain, &DOMAIN, own, &sealing).await.unwrap();
    assert_eq!(standing, Standing::Unsettled);
    let marked = Record::load(&sealing).unwrap().expect("the record stays");
    assert!(marked.reorged, "the reorg the restart saw is recorded");
    // Included again at a later block, not final yet: the next restart waits for finality.
    chain.set(|chain| {
        chain.assignments = assignments;
        chain.unfinal = true;
    });
    assert_eq!(marked.standing_at_start(&chain, &DOMAIN, own, &sealing).await.unwrap(), Standing::Unsettled);
    assert_eq!(record.standing(&chain, &DOMAIN, own).await.unwrap(), Standing::Stands, "unmarked, it would resume");
}
