//! The verified head a start after a handover seals: the handed-over checkpoint, which its replay
//! verified, while the gate applies, and none while it does not, so keys handed over once the turns
//! have ended open on a release built before the log check, nor over a range the keys name it a holder of;
//! and the one a promoted start seals: the checkpoint the promotion fixed, unless its own member
//! sequenced the range.

use super::*;

use crate::log::{CheckpointDomain, CheckpointParent, SegmentedLog};
use crate::log_check::Holder;
use crate::sequencer::ReplayedLog;
use crate::tee::keys::Head;
use crate::tee::sealing::Release;
use crate::test_support::SEGMENT_COMMANDS;

#[test]
fn a_start_after_a_handover_seals_its_check_only_while_the_gate_applies() {
    let mut node = Node::start("log-check-after-handover", &[other()]);
    node.approve(1);
    node.take_turn();
    // Another member takes the keys and the log at the last checkpoint: its replay is the check.
    let handed = handed_over(&mut node, "gated");
    assert_eq!(handed.map(|head| head.seq), Some(node.last_checkpoint().seq));
    // Once the turns have ended, the same start seals none.
    node.end_turns();
    assert_eq!(handed_over(&mut node, "ungated"), None, "keys handed over once the turns ended carry a head");
}

/// A reorged skip's member, which handed the keys back with its commands in the log under no record of
/// the log's, takes them again: the keys name it a holder past the verified head, so its start seals no
/// check over the range, while a start the holders do not name does.
#[test]
fn a_start_after_a_handover_seals_no_check_over_a_range_the_keys_name_it_a_holder_of() {
    let mut node = Node::start("log-check-after-handover-holder", &[other()]);
    node.approve(1);
    node.take_turn();
    let through = node.seq.engine.last_seq().unwrap();
    let successor = B256::from(Release { launch_digest: [0x46; 48], config_sha256: CONFIG }.id());
    let named = Holder { through, release: successor, candidates: [other().candidate].into() };
    assert_eq!(handed_over_naming(&mut node, "holder", vec![named]), None, "a holder checked its own commands");
    let elsewhere = Holder { through, release: OTHER, candidates: [AccountId([0xc9; 20])].into() };
    let checked = handed_over_naming(&mut node, "elsewhere", vec![elsewhere]);
    assert_eq!(checked.map(|head| head.seq), Some(node.last_checkpoint().seq), "another holder held the check up");
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
    // Once the turns have ended, the gate does not apply, and the start seals none.
    node.end_turns();
    node.seq.flush().unwrap();
    assert_eq!(promoted(&node, other().admin_key, fixed, "ungated"), None, "sealed with the gate off");
}

/// A promoted start reads the schedule before it starts: a member promoted within the finality window,
/// whose promotion the log has yet to mirror, sequenced the range as its promotion's commands, so its
/// own check of it counts for nothing, as one of a member the schedule never reassigned does.
#[test]
fn a_promoted_start_whose_member_the_schedule_promoted_in_the_range_seals_no_check() {
    let mut node = Node::start("log-check-after-promotion-read", &[other()]);
    node.approve(1);
    node.take_turn();
    node.seq.checkpoint().unwrap();
    node.seq.flush().unwrap();
    let fixed = node.last_checkpoint().seq;
    let reading = |name: &str, own: bool| {
        let log = recovered(&node);
        let in_office = log.replayed.takings.sequenced(None, fixed).expect("the turn taken").in_office;
        let assigned = |candidate, position, reassigned| crate::checkpoints::Assigned {
            turn: in_office.0,
            position,
            candidate,
            reassigned,
        };
        let promoted_to = if own { other().candidate } else { AccountId([0x99; 20]) };
        let read = crate::log_check::Reassigned {
            assigned: vec![assigned(in_office.1, 0, false), assigned(promoted_to, 1, true)],
            from: in_office.0,
        };
        promoted_reading(&log, other().admin_key, fixed, name, Some(&read), Vec::new())
    };
    assert_eq!(reading("read-other", false), Some(node.checkpoint_head(fixed)), "another member's promotion");
    assert_eq!(reading("read-own", true), None, "the member the schedule promoted checked its own work");
}

/// What names a promotion's turn as one the range spans is the turn that had begun by the range's last
/// command, which the live sequencer and a replay both note as they apply each command.
#[test]
fn the_live_sequencer_and_a_replay_note_the_turn_that_had_begun_by_each_command() {
    let mut node = Node::start("log-check-began", &[other()]);
    node.approve(1);
    node.take_turn();
    let own = node.turn;
    node.hand_over_turn();
    node.seq.flush().unwrap();
    let last = node.seq.engine.last_seq().unwrap();
    assert_eq!(node.seq.log_check.takings.began_by(last), Some(own + 1), "the live sequencer");
    assert_eq!(recovered(&node).replayed.takings.began_by(last), Some(own + 1), "a replay");
    assert_eq!(recovered(&node).replayed.takings.began_by(last - 1), Some(own), "the command before the boundary");
}

/// A start that read no schedule before it connected, as a whole-log take or a restart, reads the
/// reassignments of the turns from the in-office turn of the range its keys' verified head leaves through
/// the one in effect while the gate applies, so its own check sees a promotion the log has yet to mirror,
/// and reads nothing once the turns have ended.
#[tokio::test]
async fn a_start_with_no_earlier_read_reads_the_schedule_while_the_gate_applies() {
    struct Schedule(std::sync::Mutex<Vec<std::ops::Range<u64>>>);
    impl crate::checkpoints::CheckpointChain for Schedule {
        async fn state(&self, _view: crate::checkpoints::View) -> anyhow::Result<crate::checkpoints::VaultState> {
            anyhow::bail!("no vault state")
        }

        async fn post(&self, _checkpoint: &Checkpoint) -> anyhow::Result<()> {
            anyhow::bail!("posts nothing")
        }

        async fn reassignments(
            &self,
            turns: std::ops::Range<u64>,
        ) -> anyhow::Result<Vec<crate::checkpoints::Assigned>> {
            self.0.lock().unwrap().push(turns);
            Ok(Vec::new())
        }
    }
    let mut node = Node::start("log-check-start-reads", &[other()]);
    node.approve(1);
    node.take_turn();
    node.seq.flush().unwrap();
    let schedule = Schedule(Default::default());
    let read = crate::node::log_check::read_unreadied(&schedule, &recovered(&node), None, &Default::default()).await;
    let read = read.unwrap().expect("the gate applies");
    // With no verified head, the range is the holding of this node's own `TurnTaken`.
    let read_turns = schedule.0.lock().unwrap().iter().map(|turns| (turns.start, turns.end)).collect::<Vec<_>>();
    assert_eq!(read_turns, [(node.turn, node.turn + 1)]);
    assert_eq!(read.from, node.turn);
    node.end_turns();
    node.seq.flush().unwrap();
    assert_eq!(
        crate::node::log_check::read_unreadied(&schedule, &recovered(&node), None, &Default::default()).await.unwrap(),
        None
    );
    assert_eq!(schedule.0.lock().unwrap().len(), 1, "read once the turns ended");
}

/// A promoted start whose keys name its own member a holder past the verified head seals no check over
/// the range; a holder they name of another member's holds nothing up.
#[test]
fn a_promoted_start_seals_no_check_over_a_range_its_keys_name_it_a_holder_of() {
    let mut node = Node::start("log-check-after-promotion-holder", &[other()]);
    node.approve(1);
    node.take_turn();
    node.seq.checkpoint().unwrap();
    node.seq.flush().unwrap();
    let fixed = node.last_checkpoint().seq;
    let promoted_release = B256::from(Release { launch_digest: [0x47; 48], config_sha256: CONFIG }.id());
    let named = Holder { through: fixed, release: promoted_release, candidates: [other().candidate].into() };
    let holding = promoted_reading(&recovered(&node), other().admin_key, fixed, "holder", None, vec![named]);
    assert_eq!(holding, None, "a holder checked its own commands");
    let elsewhere = Holder { through: fixed, release: OTHER, candidates: [AccountId([0xc9; 20])].into() };
    let checked = promoted_reading(&recovered(&node), other().admin_key, fixed, "elsewhere", None, vec![elsewhere]);
    assert_eq!(checked, Some(node.checkpoint_head(fixed)), "another holder held the check up");
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
    crate::node::log_check::after_promotion(Some(&store), other().admin_key, &log, fixed.seq, None).unwrap();
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
    promoted_reading(&recovered(node), operator, checkpoint_seq, name, None, Vec::new())
}

/// `node`'s log as a start reads it.
fn recovered(node: &Node) -> ReplayedLog {
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let signer = AccountId(EnclaveKey::test_key().address());
    Sequencer::read_log_with_trust(&node.dir, Some(domain), Some(signer), |_| {}).unwrap()
}

/// As `promoted`, over `log`, with the schedule's reassignments `read` and the promoted member's keys
/// naming `holders`.
fn promoted_reading(
    log: &ReplayedLog,
    operator: AccountId,
    checkpoint_seq: u64,
    name: &str,
    read: Option<&crate::log_check::Reassigned>,
    holders: Vec<Holder>,
) -> Option<VerifiedHead> {
    let sealer = MockSealer { launch_digest: [0x47; 48] };
    let dir = scratch_dir(&format!("log-check-promoted-{name}"));
    let mut keys = KeySet::generate().unwrap();
    keys.holders = holders;
    let store = KeyStore::create(&dir, &sealer, CONFIG, 9, keys).unwrap();
    crate::node::log_check::after_promotion(Some(&store), operator, log, checkpoint_seq, read).unwrap();
    store.keys().log_check
}

/// The verified head the other member's start seals once it takes `node`'s keys and its log ending at
/// a checkpoint written now.
fn handed_over(node: &mut Node, name: &str) -> Option<VerifiedHead> {
    handed_over_naming(node, name, Vec::new())
}

/// `handed_over`, with the keys naming `holders`.
fn handed_over_naming(node: &mut Node, name: &str, holders: Vec<Holder>) -> Option<VerifiedHead> {
    node.seq.checkpoint().unwrap();
    node.seq.flush().unwrap();
    let last = node.last_checkpoint();
    let mut keys = node.store.keys();
    keys.predecessor_head = Some(Head { seq: last.seq, root: last.root.clone() });
    keys.holders = holders;
    let sealer = MockSealer { launch_digest: [0x46; 48] };
    let dir = scratch_dir(&format!("log-check-successor-{name}"));
    let successor = KeyStore::create(&dir, &sealer, CONFIG, 1, keys).unwrap();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let signer = AccountId(EnclaveKey::test_key().address());
    let log = Sequencer::read_log_with_trust(&node.dir, Some(domain), Some(signer), |_| {}).unwrap();
    crate::node::log_check::after_handover(Some(&successor), other().admin_key, &log, None).unwrap();
    successor.keys().log_check
}
