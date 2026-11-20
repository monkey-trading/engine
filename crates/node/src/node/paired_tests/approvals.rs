//! Under mock custody, a cache at a withdrawal approval's checkpoint owes the approval its signature,
//! which mock custody logs right after the checkpoint, whether a command or a restart that finds it
//! unsigned writes the checkpoint; a later cache owes nothing; and a restart signs nothing again.

use std::path::PathBuf;

use engine_core::event::Event;
use tokio::sync::watch;

use crate::config::NodeConfig;
use crate::log::{LogEntry, Manifest, SegmentedLog};
use crate::node::paired_tests::harness::{Node, configuration};
use crate::snapshot::{self, Owed};
use crate::test_support::{TRADER, deposit, log_entries, withdraw};

#[tokio::test]
async fn a_cache_at_an_approval_owes_its_signature_and_a_later_one_owes_nothing() {
    let node = Node::start(configuration("paired-approval-cache")).await.unwrap();
    node.submit(deposit(100, "approval-cache-deposit")).await;
    // No capture is in flight, so the approval's checkpoint is captured as it is written.
    node.history_caught_up().await;
    let events = node.submit(withdraw(40)).await;
    let event_index = events.iter().position(|event| matches!(event, Event::WithdrawalApproved { .. })).unwrap();
    node.history_caught_up().await;
    // Then a later checkpoint, captured after mock custody signed the approval.
    for n in 0..3 {
        node.submit(deposit(1, &format!("approval-cache-later-{n}"))).await;
        node.history_caught_up().await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    assert_eq!(authorizations(&cfg), 1, "mock custody signs the approval after its checkpoint");
    let caches = caches(&cfg);
    let approved_seq = caches[0].1.withdrawal_approval(TRADER, 1).expect("the withdrawal is approved").approved_seq;
    let (at_approval, engine) = caches.iter().find(|(cached, _)| cached.checkpoint.seq == approved_seq).unwrap();
    let owed = at_approval.context.payouts.as_ref().map(|payouts| payouts.owed.clone());
    assert_eq!(owed, Some(vec![Owed { account: TRADER, nonce: 1, event_index, latest: None }]));
    let restored = at_approval.context.restore_payouts(&at_approval.checkpoint, engine).unwrap();
    assert!(!restored.signed.contains(&(TRADER, 1)), "the cache owes the approval its signature");
    let (newest, _) = &caches[0];
    assert!(newest.checkpoint.seq > approved_seq, "a later checkpoint is captured");
    let owed = newest.context.payouts.as_ref().map(|payouts| payouts.owed.clone());
    assert_eq!(owed, Some(Vec::new()), "a cache after the signature owes nothing");

    let newest = newest.checkpoint.seq;
    let node = Node::start(cfg).await.unwrap();
    assert_eq!(node.snapshot_seq, Some(newest), "the restart recovers from the later cache");
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
    let (cached, engine) = caches(&cfg).remove(0);
    let approved_seq = engine.withdrawal_approval(TRADER, 1).expect("the withdrawal is approved").approved_seq;
    assert_eq!(cached.checkpoint.seq, approved_seq, "the newest cache is the approval's checkpoint");
    let owed = cached.context.payouts.as_ref().map(|payouts| payouts.owed.clone());
    assert_eq!(owed, Some(vec![Owed { account: TRADER, nonce: 1, event_index, latest: None }]));

    let node = Node::start(cfg).await.unwrap();
    assert_eq!(node.snapshot_seq, Some(approved_seq), "the next restart recovers from that cache");
    node.history_caught_up().await;
    let (cfg, _reader) = node.stop().await;
    assert_eq!(authorizations(&cfg), 1, "and signs nothing again");
}

/// The caches the node with `cfg` wrote, newest first, each with its engine.
fn caches(cfg: &NodeConfig) -> Vec<(snapshot::Snapshot, engine_core::Engine)> {
    let caches = cfg.snapshots.as_ref().unwrap();
    let read = |path: &PathBuf| {
        snapshot::read(caches, path, cfg.custody.chain_id, cfg.custody.vault.parse().unwrap()).unwrap()
    };
    snapshot::candidates_readonly(caches).unwrap().iter().map(read).collect()
}

/// The trader's authorizations the log holds.
fn authorizations(cfg: &NodeConfig) -> usize {
    log_entries(&cfg.node.log_dir)
        .iter()
        .filter(|entry| matches!(entry, LogEntry::Withdrawal(w) if w.account == TRADER && w.authorization.is_some()))
        .count()
}
