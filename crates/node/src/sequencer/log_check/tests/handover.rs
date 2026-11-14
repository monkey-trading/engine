//! The verified head a start after a handover seals: the handed-over checkpoint, which its replay
//! verified, while the gate applies, and none while it does not, so keys handed over with rotation
//! off open on a release built before the log check.

use super::*;

use crate::log::CheckpointDomain;
use crate::tee::keys::Head;

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

// ── Helpers ──

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
