//! The posted-head check through paired recovery and the start: a posted head at or under the
//! snapshot base is bound to the on-disk prefix before the snapshot is accepted, a conflict refuses
//! recovery rather than falling back, and the start then accepts the head on its seq; with no head
//! posted, recovery stamps the vault-finalized checkpoint for the stall alarm.

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

#[tokio::test]
async fn paired_recovery_stamps_the_finalized_checkpoint_for_the_stall_alarm() {
    // With `[log_heads]`, the stall alarm's anchor is the later of the checked head's stamp and the
    // vault-finalized checkpoint's. Paired recovery takes the checkpoint's from the prefix pass for
    // one under the cache's base, from the cache itself at its base, and from the suffix replay past
    // it — the value the full replay gives each time — with no head posted, or a head posted before
    // the checkpoint. Without `[log_heads]` the pass never runs for it.
    let (cfg, checkpoints, base) = super::anchor::gated_log("paired-log-head-anchor").await;
    let caches = crate::snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap();
    let newest = super::anchor::seq_of(&caches[0]);
    let in_work = checkpoints
        .iter()
        .find(|checkpoint| checkpoint.seq > base.seq && checkpoint.seq < newest)
        .expect("the newest cache carries outstanding work after its base");
    let at_cache = checkpoints.iter().find(|checkpoint| checkpoint.seq == newest).expect("the cache's checkpoint");
    let signer = Some(AccountId(EnclaveKey::test_key().address()));
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let full = |dir: &std::path::Path, parent, capture| {
        let replayed = Sequencer::read_log_with_authorities(
            dir,
            Some(domain),
            signer,
            parent,
            Default::default(),
            |_| {},
            None,
            None,
            capture,
        )
        .unwrap()
        .replayed;
        (replayed.anchor_ts, replayed.link_at)
    };
    let cases = [("base", &base), ("work", in_work), ("cache", at_cache), ("suffix", checkpoints.last().unwrap())];
    for (case, finalized) in cases {
        let mut copy = super::anchor::copy_of(&cfg, &format!("heads-{case}"));
        if case == "suffix" {
            // Recovery then uses an earlier cache, so the finalized checkpoint lies in the suffix.
            for path in crate::snapshot::candidates_readonly(copy.snapshots.as_ref().unwrap()).unwrap() {
                if super::anchor::seq_of(&path) >= finalized.seq {
                    std::fs::write(path, b"{").unwrap();
                }
            }
        }
        let parent = Some(crate::log::CheckpointParent {
            has_checkpoint: true,
            seq: finalized.seq,
            root: finalized.root.parse().unwrap(),
        });
        let recover = |copy: &crate::config::NodeConfig, posted| {
            let (log, _) = Sequencer::read_paired_log(copy, signer, parent, Default::default(), None, posted, |_| {})
                .unwrap_or_else(|error| panic!("{case}: {error:#}"));
            assert!(log.snapshot_seq.is_some(), "{case}: a paired cache serves recovery");
            log.replayed.anchor_ts
        };
        let (stamp, _) = full(&copy.node.log_dir, parent, None);
        assert!(stamp.is_some(), "{case}: the full replay stamps the finalized checkpoint");
        if case == "work" {
            assert_eq!(recover(&copy, None), None, "without [log_heads] no pass stamps it");
        }
        copy.log_heads = Some(crate::config::LogHeadsSection {
            contract: alloy::primitives::Address::repeat_byte(0xaa),
            relayer: alloy::primitives::Address::repeat_byte(0xbb),
            interval: std::time::Duration::from_secs(3600),
        });
        assert_eq!(recover(&copy, None), stamp, "{case}: no head posted");
        // This log's head posted one seq before the finalized checkpoint.
        let older = finalized.seq - 1;
        let link = full(&copy.node.log_dir, None, Some(older)).1.expect("the log holds the head's seq");
        assert_eq!(recover(&copy, Some(Head { seq: older, link })), stamp, "{case}: a head before the checkpoint");
    }
}
