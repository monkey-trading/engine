//! Ordinary sealed-approval recovery and read-only refusal of missing integrity evidence.

use super::*;

use crate::log::{CheckpointParent, SegmentedLog};

#[test]
fn durable_sealed_approval_recovers_before_checkpoint_and_retains_release_gates() {
    let mut node = Node::new("origin-real-writer-approval");
    let approval_checkpoint = node.approve();
    let approval = *node.seq.engine.withdrawal_approval(TRADER, 1).unwrap();
    let balances = node.seq.engine.accounts().clone();
    let (dir, parent) = approval_prefix(&node, "origin-durable-prefix", false);
    let (work, mut queued) = mpsc::channel(64);
    let cfg = SequencerConfig { log_dir: &dir, checkpoint_parent: parent, checkpoints: Some(work), ..node.config() };
    let (withdrawals, mut signed) = mpsc::unbounded_channel();
    let log = Sequencer::read_log_for_recovery(
        &dir,
        Some(node.domain()),
        Some(AccountId(node.seq.key.address())),
        Some(parent),
        |_| {},
    )
    .unwrap();
    let mut recovered =
        Sequencer::recover_from(log, cfg, node.seq.key.clone(), broadcast::channel(128).0, withdrawals).unwrap();
    assert_eq!(recovered.engine.withdrawal_approval(TRADER, 1), Some(&approval));
    assert_eq!(recovered.engine.accounts(), &balances);
    assert!(signed.try_recv().is_err(), "recovery integrity alone grants no payout authority");
    assert!(queued.try_recv().is_err(), "unvalidated collateral cannot produce a successor signature");
    let anchors = recovered.take_recovery_checkpoints();
    assert!(
        anchors.iter().any(|work| work.checkpoint.seq == parent.seq),
        "the worker receives the finalized anchor accounting"
    );
    recovered.checkpoint_status(Some(parent.seq), Some(crate::checkpoints::Health::Pending)).unwrap();
    recovered.checkpoint().unwrap();
    let mut recovered_checkpoint = None;
    while let Ok(work) = queued.try_recv() {
        if work.checkpoint.seq == approval.approved_seq {
            recovered_checkpoint = Some(work.checkpoint);
        }
    }
    let checkpoint = recovered_checkpoint.expect("recovered sealed approval extends its existing parent");
    assert_eq!(checkpoint.parent, Some(parent));
    assert_eq!(checkpoint.root, approval_checkpoint.root);
    recovered.trust(sample_around(node.clock.as_ref())).unwrap();
    let evidence = node.evidence(&checkpoint);
    recovered.release_payouts(&evidence).unwrap();
    assert!(signed.try_recv().is_err(), "unreconciled custody cannot release a recovered approval");
    recovered.checkpoint_status(Some(checkpoint.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    let mut not_included = evidence.clone();
    not_included.checkpoint_seq = Some(checkpoint.seq - 1);
    recovered.release_payouts(&not_included).unwrap();
    assert!(signed.try_recv().is_err(), "origin evidence cannot replace finalized approval inclusion");
    recovered.release_payouts(&evidence).unwrap();
    recovered.flush().unwrap();
    let released = signed.try_recv().unwrap();
    assert_eq!((released.account, released.nonce, released.amount), (TRADER, approval.nonce, approval.amount));
    crate::withdrawals::verify(&released, node.domain(), AccountId(node.seq.key.address())).unwrap();
    assert!(signed.try_recv().is_err());
    assert_eq!(recovered.engine.accounts(), &balances, "recovery never repeats the reservation debit");
}

#[test]
fn missing_seal_refuses_recovery_without_opening_writer_or_producing_signatures() {
    let mut node = Node::new("origin-missing-evidence");
    node.approve();
    let (dir, parent) = approval_prefix(&node, "origin-missing-prefix", true);
    let before = files(&dir);
    let (work, mut queued) = mpsc::channel(64);
    let cfg = SequencerConfig { log_dir: &dir, checkpoint_parent: parent, checkpoints: Some(work), ..node.config() };
    let log = Sequencer::read_log_for_recovery(
        &dir,
        Some(node.domain()),
        Some(AccountId(node.seq.key.address())),
        Some(parent),
        |_| {},
    )
    .unwrap();
    let error = log.validate_recovery(&cfg, AccountId(node.seq.key.address())).unwrap_err();
    assert!(error.to_string().contains("unauthenticated command tail"), "{error:#}");
    let (withdrawals, mut signed) = mpsc::unbounded_channel();
    let result = Sequencer::recover_from(log, cfg, node.seq.key.clone(), broadcast::channel(128).0, withdrawals);
    assert!(result.err().unwrap().to_string().contains("unauthenticated command tail"));
    assert!(queued.try_recv().is_err());
    assert!(signed.try_recv().is_err());
    assert_eq!(files(&dir), before, "refusal preserves every log/manifest byte");
}

/// Copies actual admitted writer entries through an approval, before its checkpoint entry.
fn approval_prefix(node: &Node, name: &str, omit_last_seal: bool) -> (PathBuf, CheckpointParent) {
    let entries = log_entries(&node.dir);
    let last = entries
        .iter()
        .rposition(|entry| {
            matches!(entry, LogEntry::Command(command)
            if matches!(command.sequenced.cmd, Command::Withdraw { .. }))
        })
        .unwrap();
    let mut prefix = entries[..=last].to_vec();
    let LogEntry::Command(command) = prefix.last_mut().unwrap() else { unreachable!() };
    assert!(command.seal.is_some(), "the actual sequencer writer must seal the approval entry");
    if omit_last_seal {
        command.seal = None;
    }
    let parent = prefix
        .iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => {
                Some(CheckpointParent { has_checkpoint: true, seq: cp.seq, root: cp.root.parse().unwrap() })
            }
            _ => None,
        })
        .unwrap();
    let dir = scratch_dir(name);
    let mut writer = SegmentedLog::open(&dir, |_| Ok(()))
        .unwrap()
        .into_writer(10_000, watch::channel(Manifest::default()).0)
        .unwrap();
    for entry in &prefix {
        writer.append(entry).unwrap();
    }
    writer.sync().unwrap();
    drop(writer);
    (dir, parent)
}

/// Captures persisted bytes without opening any writable recovery component.
fn files(dir: &std::path::Path) -> BTreeMap<PathBuf, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

/// A node under a new key, as after a rotation, refuses the old key's checkpoints that the vault
/// never accepted, even in a drained log that ends at one.
#[test]
fn a_new_key_refuses_old_key_checkpoints_the_vault_never_accepted() {
    let mut node = Node::new("origin-new-key-unaccepted-tail");
    let accepted = node.approve();
    node.seq.sequence(Command::Tick).unwrap().unwrap();
    node.seq.checkpoint().unwrap();
    node.seq.flush().unwrap();
    let checkpoints: Vec<_> = log_entries(&node.dir)
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(cp),
            _ => None,
        })
        .collect();
    let last = checkpoints.last().unwrap().clone();
    assert!(last.seq > accepted.seq, "the drained log ends at a checkpoint after the accepted one");
    let signer = AccountId(EnclaveKey::generate().address());
    let history = |through: u64| SignerHistory {
        checkpoints: checkpoints
            .iter()
            .filter(|cp| cp.seq <= through)
            .map(|cp| AcceptedCheckpoint {
                seq: cp.seq,
                root: cp.root.clone(),
                signer: cp.signer.parse().unwrap(),
                signer_epoch: 1,
            })
            .collect(),
        epochs: BTreeMap::from([(1, AccountId(node.seq.key.address())), (2, signer)]),
    };
    let anchor =
        |cp: &Checkpoint| CheckpointParent { has_checkpoint: true, seq: cp.seq, root: cp.root.parse().unwrap() };
    let read = |cp: &Checkpoint| {
        Sequencer::read_log_with_authorities(
            &node.dir,
            Some(node.domain()),
            Some(signer),
            Some(anchor(cp)),
            history(cp.seq),
            |_| {},
        )
    };
    let error = read(&accepted).err().unwrap();
    assert!(format!("{error:#}").contains("trusted recovery authority"), "{error:#}");
    read(&last).expect("the same log recovers once the vault accepted its last checkpoint");
}
