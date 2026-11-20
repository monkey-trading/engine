//! Under mock custody, a cache at a withdrawal approval's checkpoint owes the approval its signature,
//! which mock custody logs right after the checkpoint, whether a command or a restart that finds it
//! unsigned writes the checkpoint, and a restart from that cache signs nothing again.

use engine_core::event::Event;
use tokio::sync::watch;

use crate::config::NodeConfig;
use crate::log::{LogEntry, Manifest, SegmentedLog};
use crate::node::paired_tests::harness::{Node, configuration};
use crate::snapshot::{self, Owed};
use crate::test_support::{TRADER, deposit, log_entries, withdraw};

#[tokio::test]
async fn a_cache_at_an_approval_owes_its_signature_and_a_restart_signs_nothing_again() {
    let node = Node::start(configuration("paired-approval-cache")).await.unwrap();
    node.submit(deposit(100, "approval-cache-deposit")).await;
    // No capture is in flight, so the approval's checkpoint is captured as it is written.
    node.history_caught_up().await;
    let events = node.submit(withdraw(40)).await;
    let event_index = events.iter().position(|event| matches!(event, Event::WithdrawalApproved { .. })).unwrap();
    node.history_caught_up().await;
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let caches = cfg.snapshots.as_ref().unwrap();
    let vault = cfg.custody.vault.parse().unwrap();
    let path = snapshot::candidates_readonly(caches).unwrap().remove(0);
    let (cached, engine) = snapshot::read(caches, &path, cfg.custody.chain_id, vault).unwrap();
    let approved_seq = engine.withdrawal_approval(TRADER, 1).expect("the withdrawal is approved").approved_seq;
    assert_eq!(cached.checkpoint.seq, approved_seq, "the newest cache is the approval's checkpoint");
    let owed = cached.context.payouts.as_ref().map(|payouts| payouts.owed.clone());
    assert_eq!(owed, Some(vec![Owed { account: TRADER, nonce: 1, event_index, latest: None }]));
    let restored = cached.context.restore_payouts(&cached.checkpoint, &engine).unwrap();
    assert!(!restored.signed.contains(&(TRADER, 1)), "the cache owes the approval its signature");
    assert_eq!(authorizations(&cfg), 1, "mock custody signs the approval after its checkpoint");

    let node = Node::start(cfg).await.unwrap();
    assert_eq!(node.snapshot_seq, Some(approved_seq), "the restart recovers from the approval's cache");
    node.history_caught_up().await;
    let (cfg, _reader) = node.stop().await;
    assert_eq!(authorizations(&cfg), 1, "the restart signs nothing again");
}

#[tokio::test]
async fn a_restart_that_signs_an_unsigned_approval_owes_it_in_the_cache_it_takes() {
    let node = Node::start(configuration("paired-approval-unsigned-source")).await.unwrap();
    node.submit(deposit(100, "unsigned-deposit")).await;
    let events = node.submit(withdraw(40)).await;
    let event_index = events.iter().position(|event| matches!(event, Event::WithdrawalApproved { .. })).unwrap();
    let (source, reader) = node.stop().await;
    drop(reader);
    // The log as a crash after the approval's command, before its checkpoint and signature, leaves it.
    let entries = log_entries(&source.node.log_dir);
    let approving = entries.iter().rposition(|entry| matches!(entry, LogEntry::Command(_))).unwrap();
    let cfg = configuration("paired-approval-unsigned");
    let manifest = watch::channel(Manifest::default()).0;
    let opened = SegmentedLog::open(&cfg.node.log_dir, |_| Ok(())).unwrap();
    let mut writer = opened.into_writer(cfg.node.segment_commands, manifest).unwrap();
    entries[..=approving].iter().for_each(|entry| _ = writer.append(entry).unwrap());
    writer.sync().unwrap();
    drop(writer);
    assert_eq!(authorizations(&cfg), 0, "the approval is unsigned");

    // The restart writes the approval's checkpoint, which is captured before the approval is signed.
    let node = Node::start(cfg).await.unwrap();
    node.history_caught_up().await;
    let (cfg, reader) = node.stop().await;
    drop(reader);
    assert_eq!(authorizations(&cfg), 1, "the restart signs the approval");
    let (cached, approved_seq) = newest(&cfg);
    assert_eq!(cached.checkpoint.seq, approved_seq, "the newest cache is the approval's checkpoint");
    let owed = cached.context.payouts.as_ref().map(|payouts| payouts.owed.clone());
    assert_eq!(owed, Some(vec![Owed { account: TRADER, nonce: 1, event_index, latest: None }]));

    let node = Node::start(cfg).await.unwrap();
    assert_eq!(node.snapshot_seq, Some(approved_seq), "the next restart recovers from that cache");
    node.history_caught_up().await;
    let (cfg, _reader) = node.stop().await;
    assert_eq!(authorizations(&cfg), 1, "and signs nothing again");
}

/// The newest cache the node with `cfg` wrote, and the seq of the trader's approval its engine holds.
fn newest(cfg: &NodeConfig) -> (snapshot::Snapshot, u64) {
    let caches = cfg.snapshots.as_ref().unwrap();
    let path = snapshot::candidates_readonly(caches).unwrap().remove(0);
    let (cached, engine) =
        snapshot::read(caches, &path, cfg.custody.chain_id, cfg.custody.vault.parse().unwrap()).unwrap();
    let approved_seq = engine.withdrawal_approval(TRADER, 1).expect("the withdrawal is approved").approved_seq;
    (cached, approved_seq)
}

/// The trader's authorizations the log holds.
fn authorizations(cfg: &NodeConfig) -> usize {
    log_entries(&cfg.node.log_dir)
        .iter()
        .filter(|entry| matches!(entry, LogEntry::Withdrawal(w) if w.account == TRADER && w.authorization.is_some()))
        .count()
}
