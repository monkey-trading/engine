//! The verified head a start after a handover seals: the handed-over checkpoint, which its replay
//! verified, while the gate applies, and none while it does not, so keys handed over with rotation
//! off open on a release built before the log check; and the one a promoted start seals: the
//! checkpoint the promotion fixed, unless its own member sequenced the range.

use super::*;

use crate::log::{CheckpointDomain, CheckpointParent, SegmentedLog};
use crate::tee::keys::Head;
use crate::test_support::SEGMENT_COMMANDS;

#[test]
fn a_start_after_a_handover_seals_its_check_only_while_the_gate_applies() {
    let mut node = Node::start("log-check-after-handover", &[other()]);
    node.approve(1);
    node.take_turn();
    // Another member takes the keys and the log at the last checkpoint: its replay is the check.
    let handed = handed_over(&mut node, "gated");
    assert_eq!(handed.map(|head| head.seq), Some(node.last_checkpoint().seq));
    // With rotation off, the same start seals none.
    let off =
        Command::SwitchRotation { enabled: false, election_cancelled: false, block: 14, chain_ref: "0x04:0".into() };
    node.seq.sequence(off).unwrap().expect("admitted");
    assert_eq!(handed_over(&mut node, "ungated"), None, "keys handed over with rotation off carry a head");
}

#[test]
fn a_promoted_start_seals_its_check_only_when_another_member_sequenced_the_range() {
    let mut node = Node::start("log-check-after-promotion", &[other()]);
    node.approve(1);
    node.take_turn();
    node.seq.checkpoint().unwrap();
    node.seq.flush().unwrap();
    let fixed = node.last_checkpoint().seq;
    // The other member, promoted over this node's turn, replayed its log through the fixed checkpoint.
    let other_member = promoted(&node, other().admin_key, fixed, "other");
    assert_eq!(other_member, Some(node.checkpoint_head(fixed)), "the promoted member's replay is the check");
    // This node's own member, promoted under another release, sequenced the range: its replay is none.
    assert_eq!(promoted(&node, node.member, fixed, "own"), None, "the member that sequenced it checked itself");
    // With rotation off, the gate does not apply, and the start seals none.
    let off =
        Command::SwitchRotation { enabled: false, election_cancelled: false, block: 14, chain_ref: "0x04:0".into() };
    node.seq.sequence(off).unwrap().expect("admitted");
    node.seq.flush().unwrap();
    assert_eq!(promoted(&node, other().admin_key, fixed, "ungated"), None, "sealed with the gate off");
}

/// A promotion can fix a checkpoint the vault accepted whose entry no member received: the promoted
/// start anchors at its own state there, and that root is the one its check seals and a restart's log
/// check restores the verified head against.
#[test]
fn a_promoted_start_checks_through_a_fixed_checkpoint_whose_entry_its_log_lacks() {
    let mut node = Node::start("log-check-after-promotion-unheld", &[other()]);
    node.approve(1);
    node.take_turn();
    node.seq.checkpoint().unwrap();
    node.seq.flush().unwrap();
    let fixed = node.last_checkpoint();
    let copy = scratch_dir("log-check-promoted-unheld-copy");
    let (manifest, _) = tokio::sync::watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&copy, |_| Ok(())).unwrap().into_writer(SEGMENT_COMMANDS, manifest).unwrap();
    for entry in log_entries(&node.dir) {
        if !matches!(&entry, LogEntry::Checkpoint(checkpoint) if checkpoint.seq == fixed.seq) {
            writer.append(&entry).unwrap();
        }
    }
    writer.sync().unwrap();
    let parent = CheckpointParent { has_checkpoint: true, seq: fixed.seq, root: fixed.root.parse().unwrap() };
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let signer = AccountId(EnclaveKey::test_key().address());
    let log = Sequencer::read_log_for_recovery(&copy, Some(domain), Some(signer), Some(parent), |_| {}).unwrap();
    let sealer = MockSealer { launch_digest: [0x48; 48] };
    let dir = scratch_dir("log-check-promoted-unheld-keys");
    let store = Arc::new(KeyStore::create(&dir, &sealer, CONFIG, 9, KeySet::generate().unwrap()).unwrap());
    crate::node::log_check::after_promotion(Some(&store), other().admin_key, &log, fixed.seq).unwrap();
    let head = node.checkpoint_head(fixed.seq);
    assert_eq!(store.keys().log_check, Some(head), "the promotion's check verified the log through it");
    let setup = LogCheckSetup { store, due: tokio::sync::watch::channel(None).0 };
    let restored = LogCheck::recovered(Some(setup), other().admin_key, &log.replayed);
    assert_eq!(restored.verified, Some(head), "a restart restores the head the check sealed");
}

// ── Helpers ──

/// The verified head the other member's promoted start under `operator` seals over `node`'s log once a
/// promotion fixed its checkpoint at `checkpoint_seq`.
fn promoted(node: &Node, operator: AccountId, checkpoint_seq: u64, name: &str) -> Option<VerifiedHead> {
    let sealer = MockSealer { launch_digest: [0x47; 48] };
    let dir = scratch_dir(&format!("log-check-promoted-{name}"));
    let store = KeyStore::create(&dir, &sealer, CONFIG, 9, KeySet::generate().unwrap()).unwrap();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let signer = AccountId(EnclaveKey::test_key().address());
    let log = Sequencer::read_log_with_trust(&node.dir, Some(domain), Some(signer), |_| {}).unwrap();
    crate::node::log_check::after_promotion(Some(&store), operator, &log, checkpoint_seq).unwrap();
    store.keys().log_check
}

/// The verified head the other member's start seals once it takes `node`'s keys and its log ending at
/// a checkpoint written now.
fn handed_over(node: &mut Node, name: &str) -> Option<VerifiedHead> {
    node.seq.checkpoint().unwrap();
    node.seq.flush().unwrap();
    let last = node.last_checkpoint();
    let mut keys = node.store.keys();
    keys.predecessor_head = Some(Head { seq: last.seq, root: last.root.clone() });
    let sealer = MockSealer { launch_digest: [0x46; 48] };
    let dir = scratch_dir(&format!("log-check-successor-{name}"));
    let successor = KeyStore::create(&dir, &sealer, CONFIG, 1, keys).unwrap();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let signer = AccountId(EnclaveKey::test_key().address());
    let log = Sequencer::read_log_with_trust(&node.dir, Some(domain), Some(signer), |_| {}).unwrap();
    crate::node::log_check::after_handover(Some(&successor), other().admin_key, &log).unwrap();
    successor.keys().log_check
}
