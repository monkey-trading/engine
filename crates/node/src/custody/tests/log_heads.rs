//! Log-head reads through the custody quorum: block and head at the highest height a strict
//! majority agrees on, never the custody finality tag, falling back past a faulty provider, a
//! lagging minority excluded, and one quorum loss when no height wins.

use alloy::primitives::{Address, B256};

use crate::custody::tests::fakes::*;
use crate::custody::tests::quorum::{chains, quorum};

#[tokio::test]
async fn the_log_head_read_uses_a_majority_latest_block_not_the_finality_tag() {
    let chains = chains();
    // A post lands at block five; the chain's head is ten and the finality tag still names block
    // one, as a fresh chain's does. The read must see the post at the latest block — a finality-
    // tagged read would ask block one, before the post, and answer no head.
    for chain in &chains {
        chain.0.lock().unwrap().head = 10;
        chain.0.lock().unwrap().epoch = 1;
        chain.0.lock().unwrap().posted = Some((7, B256::from([7; 32])));
        chain.0.lock().unwrap().posted_block = Some(5);
    }
    let quorum = quorum(&chains).await;
    let (epoch, head) = crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.unwrap();
    assert_eq!(epoch, 1);
    assert_eq!(head, Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) }));
    for chain in &chains {
        assert_eq!(
            chain.0.lock().unwrap().head_reads,
            vec![(10, canonical_hash(10))],
            "the head is read at the majority-agreed latest block, never the finality tag"
        );
    }
}

#[tokio::test]
async fn a_lagging_latest_block_minority_cannot_vote_on_the_log_head_read() {
    let chains = chains();
    for chain in &chains {
        chain.0.lock().unwrap().head = 10;
        chain.0.lock().unwrap().epoch = 1;
    }
    // The majority holds the newer head, visible from block three; the minority still sees block
    // nine and the older head it held there. Both votes take the majority's answer, read at block
    // ten: a finality-tagged read would ask block one and see neither post.
    for chain in &chains[..2] {
        chain.0.lock().unwrap().posted = Some((7, B256::from([7; 32])));
        chain.0.lock().unwrap().posted_block = Some(3);
    }
    chains[2].0.lock().unwrap().latest = Some((9, canonical_hash(9)));
    chains[2].0.lock().unwrap().posted = Some((5, B256::from([5; 32])));
    chains[2].0.lock().unwrap().posted_block = Some(2);
    let quorum = quorum(&chains).await;
    let (_, head) = crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.unwrap();
    assert_eq!(head, Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) }));
    for chain in &chains[..2] {
        assert_eq!(chain.0.lock().unwrap().head_reads, vec![(10, canonical_hash(10))]);
    }
}

#[tokio::test]
async fn providers_at_different_tips_agree_at_the_majoritys_common_height() {
    // Polled together on a fast chain, three providers answer three tips: the read votes the hash
    // at the second-highest, which a strict majority has reached, and reads the head there.
    let chains = chains();
    for (chain, head) in chains.iter().zip([10, 11, 12]) {
        let mut state = chain.0.lock().unwrap();
        state.head = head;
        state.epoch = 1;
        state.posted = Some((7, B256::from([7; 32])));
        state.posted_block = Some(3);
    }
    let quorum = quorum(&chains).await;
    let (_, head) = crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.unwrap();
    assert_eq!(head, Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) }));
    for chain in &chains {
        assert_eq!(chain.0.lock().unwrap().head_reads, vec![(11, canonical_hash(11))], "read at the common height");
    }
}

#[tokio::test]
async fn a_faulty_high_tip_cannot_deny_the_read_when_honest_providers_agree_lower() {
    // Honest tips 10 and 11; a faulty provider claims 999 and fails its header read at 11, the
    // highest height a majority has reached. No majority agrees there, so the read falls to 10,
    // where both honest providers agree.
    let chains = chains();
    for (chain, head) in chains.iter().zip([10, 11, 999]) {
        let mut state = chain.0.lock().unwrap();
        state.head = head;
        state.epoch = 1;
        state.posted = Some((7, B256::from([7; 32])));
        state.posted_block = Some(3);
    }
    let quorum = quorum(&chains).await;
    chains[2].0.lock().unwrap().header_failures = 1;
    let (_, head) = crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.unwrap();
    assert_eq!(head, Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) }));
    for chain in &chains[..2] {
        assert_eq!(chain.0.lock().unwrap().head_reads, vec![(10, canonical_hash(10))], "read at the agreed height");
    }
}

#[tokio::test]
async fn a_faulty_provider_refusing_the_head_cannot_deny_the_read_when_honest_providers_agree_lower() {
    // Honest tips 10 and 11; a faulty provider claims 999, agrees on the block hash at 11, then
    // refuses the head read. The provider at 10 cannot serve block 11, so no majority reads the head
    // there; the read falls to 10, where both honest providers read it.
    let chains = chains();
    for (chain, head) in chains.iter().zip([10, 11, 999]) {
        let mut state = chain.0.lock().unwrap();
        state.head = head;
        state.epoch = 1;
        state.posted = Some((7, B256::from([7; 32])));
        state.posted_block = Some(3);
    }
    let quorum = quorum(&chains).await;
    chains[2].0.lock().unwrap().refuse_head_reads = true;
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (_, head) = crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.unwrap();
    assert_eq!(head, Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) }));
    for chain in &chains[..2] {
        assert_eq!(chain.0.lock().unwrap().head_reads.last(), Some(&(10, canonical_hash(10))), "read at 10");
    }
    // The losing candidate voted silently: no quorum loss pages for a read that succeeded.
    let lost: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|alert| alert.get("alert").map(String::as_str) == Some("custody_rpc_quorum_lost"))
        .collect();
    assert!(lost.is_empty(), "a candidate that loses raises nothing: {lost:?}");
}

#[tokio::test]
async fn no_candidate_height_with_a_majority_fails_the_read_with_one_quorum_loss() {
    // Every provider is at a different tip and each refuses the header reads below its tip: no
    // height has a strict majority on the block, so the read fails and pages quorum loss once.
    let chains = chains();
    for (chain, head) in chains.iter().zip([10, 11, 12]) {
        let mut state = chain.0.lock().unwrap();
        state.head = head;
        state.epoch = 1;
    }
    let quorum = quorum(&chains).await;
    for chain in &chains {
        chain.0.lock().unwrap().header_failures = u32::MAX;
    }
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    assert!(crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.is_err());
    let lost: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|alert| alert.get("alert").map(String::as_str) == Some("custody_rpc_quorum_lost"))
        .collect();
    assert_eq!(lost.len(), 1, "one quorum loss for the read: {lost:?}");
    assert_eq!(lost[0].get("operation").map(String::as_str), Some("latest_block"));
}
