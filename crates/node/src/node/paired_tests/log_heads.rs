//! The posted-head check through paired recovery and the start: a posted head at or under the
//! snapshot base is bound to the on-disk prefix before the snapshot is accepted, a conflict refuses
//! recovery rather than falling back, and the start then accepts the head on its seq.

use alloy::primitives::B256;
use engine_core::types::AccountId;

use crate::log::CheckpointDomain;
use crate::log_heads::Head;
use crate::log_heads::prefix::Rollback;
use crate::node::paired_tests::harness::{Node, configuration};
use crate::sequencer::Sequencer;
use crate::tee::EnclaveKey;
use crate::test_support::deposit;

#[tokio::test]
async fn paired_recovery_binds_a_posted_head_under_the_base_to_the_on_disk_prefix() {
    let node = Node::start(configuration("paired-log-head")).await.unwrap();
    for i in 0..8 {
        node.submit(deposit(1, &format!("log-head-{i}"))).await;
    }
    node.history_caught_up().await;
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let signer = Some(AccountId(EnclaveKey::test_key().address()));
    let rpc_dir = crate::test_support::scratch_dir("paired-log-head-rpc");
    std::fs::create_dir_all(&rpc_dir).unwrap();
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let recover =
        |posted: Option<Head>| Sequencer::read_paired_log(&cfg, signer, None, Default::default(), None, posted, |_| {});
    let base = recover(None).unwrap().0.snapshot_seq.expect("recovery uses a paired cache");
    // The prefix the log holds at each seq under the base, and that command's sequenced time,
    // from a full replay of the same bytes.
    let replayed_at = |seq| {
        let replayed = Sequencer::read_log_with_authorities(
            &cfg.node.log_dir,
            Some(domain),
            signer,
            None,
            Default::default(),
            |_| {},
            None,
            None,
            Some(seq),
        )
        .unwrap()
        .replayed;
        (replayed.link_at.expect("the log holds a command at the seq"), replayed.ts_at)
    };
    for seq in [base, base - 1] {
        let (link, ts) = replayed_at(seq);
        let held = Head { seq, link };
        let (log, _) = recover(Some(held)).expect("the prefix this log holds is accepted");
        assert_eq!(log.snapshot_seq, Some(base), "the snapshot still serves recovery");
        assert_eq!(log.replayed.ts_at, ts, "the prefix pass carries the posted command's time to the stall alarm");
        // The start then accepts the head on its seq: the suffix replay never reaches it, and
        // recovery bound it to the on-disk prefix.
        let pending = crate::log_heads::setup::Pending::for_test(Some(held), &rpc_dir);
        assert!(crate::log_heads::setup::startup(Some(pending), &log).is_ok(), "the start accepts seq {seq}");
        let foreign = Head { seq, link: B256::from([0x5a; 32]) };
        let error = recover(Some(foreign)).err().expect("a foreign prefix under the base refuses recovery");
        assert!(error.is::<Rollback>(), "a refusal, not a rejected cache to fall back from: {error:#}");
    }
}
