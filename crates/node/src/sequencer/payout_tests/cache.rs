//! A restart from a recovery cache taken after a withdrawal approval holds the payout full replay
//! does: it pays the approval, and signs it again only in a signer epoch it was not signed in.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use super::*;
use crate::checkpoints::Health;
use crate::log::{CheckpointParent, SegmentedLog};
use crate::sequencer::ReplayedLog;
use crate::sequencer::replay::Replay;
use crate::snapshot;
use crate::test_support::copy_dir;

/// A change to a cache's payouts, given its checkpoint.
type Tamper = fn(&mut snapshot::Payouts, &Checkpoint);

/// A restart from a cache taken before the approval's authorization was logged, which replays one
/// naming a checkpoint before the cache, and from one taken after it, holds the payout full replay
/// does, signs nothing again, and retires the approval once custody sees it claimed. On the root
/// worker, the earlier cache is taken while the approval's proof is still being taken.
#[test]
fn a_restart_from_a_cache_pays_its_approval_and_signs_nothing_again() {
    for detached in [false, true] {
        let mut node = Node::new(&format!("payout-cache-restart-{detached}"));
        if detached {
            node.seq.start_root_worker().unwrap();
        }
        let approved = node.approve();
        let early = node.later_checkpoint("early");
        let early = node.capture(&early);
        node.seq.checkpoint_status(Some(approved.seq), Some(Health::Healthy)).unwrap();
        node.seq.release_payouts(&node.evidence(&approved)).unwrap();
        node.seq.flush().unwrap();
        let authorized = node.signed.try_recv().expect("the approval is authorized");
        assert_eq!(authorized.authorization.as_ref().unwrap().checkpoint_seq, approved.seq);
        let late = node.later_checkpoint("late");
        let late = node.capture(&late);
        let full = node.recover_copy(None, &approved, "full");
        let expected = full.seq.account_payouts(TRADER, None, 10);
        assert_eq!(expected.payouts[0].withdrawal.as_ref(), Some(&authorized), "full replay holds the authorization");
        for (label, cache) in [("early", early), ("late", late)] {
            let mut restarted = node.recover_copy(Some(&cache), &approved, label);
            assert_eq!(
                restarted.seq.account_payouts(TRADER, None, 10),
                expected,
                "{label}: the payout full replay holds"
            );
            let unclaimed = restarted.posted(&approved);
            restarted.seq.release_payouts(&unclaimed).unwrap();
            restarted.seq.flush().unwrap();
            assert!(restarted.signed.try_recv().is_err(), "{label}: an authorization in force is not signed again");
            let mut claimed = unclaimed;
            claimed.approvals[0].claimed = true;
            restarted.seq.release_payouts(&claimed).unwrap();
            assert!(
                restarted.seq.account_payouts(TRADER, None, 10).payouts.is_empty(),
                "{label}: the claim retires it"
            );
            assert_eq!(authorizations(&restarted.dir), 1, "{label}: the log holds one authorization");
        }
    }
}

/// A restart from a cache taken before a new signer epoch signed the approval again, and from one
/// taken after, signs nothing in that epoch, and signs once in the next.
#[test]
fn a_restart_from_a_cache_signs_again_only_in_a_new_signer_epoch() {
    let mut node = Node::new("payout-cache-epochs");
    let approved = node.approve();
    node.seq.checkpoint_status(Some(approved.seq), Some(Health::Healthy)).unwrap();
    node.seq.release_payouts(&node.evidence(&approved)).unwrap();
    let before = node.later_checkpoint("before");
    let before = node.capture(&before);
    let mut rotated = node.evidence(&approved);
    rotated.signer_epoch = 2;
    node.seq.release_payouts(&rotated).unwrap();
    node.seq.flush().unwrap();
    assert_eq!(authorizations(&node.dir), 2, "the new epoch signs the unpaid approval again");
    let after = node.later_checkpoint("after");
    let after = node.capture(&after);
    for (label, cache) in [("before", before), ("after", after)] {
        let mut restarted = node.recover_copy(Some(&cache), &approved, label);
        let mut evidence = restarted.posted(&approved);
        evidence.signer_epoch = 2;
        restarted.seq.release_payouts(&evidence).unwrap();
        restarted.seq.flush().unwrap();
        assert!(restarted.signed.try_recv().is_err(), "{label}: epoch 2's authorization is not signed again");
        evidence.signer_epoch = 3;
        restarted.seq.release_payouts(&evidence).unwrap();
        restarted.seq.release_payouts(&evidence).unwrap();
        restarted.seq.flush().unwrap();
        let reissued = restarted.signed.try_recv().expect("the next epoch signs it again");
        assert_eq!(reissued.authorization.as_ref().unwrap().signer_epoch, 3, "{label}");
        assert!(restarted.signed.try_recv().is_err(), "{label}: once");
        assert_eq!(authorizations(&restarted.dir), 3, "{label}");
    }
}

/// A cache whose payouts do not restore against its engine is refused, so recovery falls back:
/// one that carries none while its engine holds an approval, as a cache from before payouts were
/// cached would, and one whose owed approvals or roots contradict it.
#[test]
fn a_cache_whose_payouts_contradict_its_engine_is_refused() {
    let mut node = Node::new("payout-cache-refused");
    let first = node.approve();
    let later = AccountId([1; 20]);
    node.seq
        .sequence(Command::Deposit { account: later, amount: Fixed::from_int(100), chain_ref: "cache-later".into() })
        .unwrap()
        .unwrap();
    node.seq.sequence(Command::Withdraw { account: later, amount: Fixed::from_int(10), auth: None }).unwrap().unwrap();
    node.seq.checkpoint_status(Some(first.seq), Some(Health::Healthy)).unwrap();
    node.seq.release_payouts(&node.evidence(&first)).unwrap();
    let refused = node.later_checkpoint("refused");
    let path = node.capture(&refused);
    let (cache, domain) = (node.cache(), node.domain());
    let read = || snapshot::read(&cache, &path, domain.chain_id, domain.vault).unwrap();
    let (cached, _) = read();
    let payouts = cached.context.payouts.clone().expect("the cache owes its approvals");
    assert_eq!(payouts.owed.len(), 2, "both approvals are unpaid");
    assert!(payouts.owed[0].latest.is_some() && payouts.owed[1].latest.is_none(), "the first is authorized");
    let (checkpoint, signer) = (cached.checkpoint.clone(), AccountId(node.seq.key.address()));
    let cases: [(&str, Tamper, &str); 6] = [
        ("absent", |_, _| {}, "does not carry"),
        ("twice", |payouts, _| payouts.owed.push(payouts.owed[0].clone()), "listed twice"),
        ("reordered", |payouts, _| payouts.owed.reverse(), "out of payout order"),
        ("unapproved", |payouts, _| payouts.owed[1].nonce = 2, "not an engine approval"),
        ("foreign", |payouts, _| payouts.owed[1].latest = payouts.owed[0].latest.clone(), "not its approval's"),
        ("root", |payouts, cp| _ = payouts.roots.insert(cp.seq, B256::ZERO), "not before its checkpoint"),
    ];
    for (label, tamper, refusal) in cases {
        let (mut tampered, _) = read();
        let mut changed = payouts.clone();
        tamper(&mut changed, &checkpoint);
        tampered.context.payouts = (label != "absent").then_some(changed);
        let dir = PathBuf::from(format!("{}-{label}", cache.dir.display()));
        let tampered_cache = crate::config::SnapshotSection { dir: dir.clone(), ..cache.clone() };
        snapshot::write(&tampered_cache, &tampered, &node.seq.key).unwrap();
        let (tampered, engine) =
            snapshot::read(&tampered_cache, &dir.join(snapshot::name(tampered.offset)), domain.chain_id, domain.vault)
                .unwrap();
        let error = Replay::from_snapshot(engine, tampered, domain, Some(signer), Some(parent(&first))).err().unwrap();
        assert!(error.to_string().contains(refusal), "{label}: {error:#}");
    }
}

impl Node {
    /// Where this node's caches are written, pinned to its key.
    fn cache(&self) -> crate::config::SnapshotSection {
        crate::config::SnapshotSection {
            dir: PathBuf::from(format!("{}-cache", self.dir.display())),
            retain: NonZeroUsize::new(8).unwrap(),
            interval: std::time::Duration::ZERO,
            max_bytes: NonZeroUsize::new(32 * 1024 * 1024).unwrap(),
            trusted_signers: vec![AccountId(self.seq.key.address())],
        }
    }

    /// A deposit and a checkpoint after it, the checkpoint returned.
    fn later_checkpoint(&mut self, label: &str) -> Checkpoint {
        self.seq.sequence(deposit(1, &format!("payout-cache-{label}"))).unwrap().unwrap();
        self.seq.checkpoint().unwrap();
        self.last_checkpoint()
    }

    /// Writes the cache a capture takes at `cp`, the checkpoint just written, and gives its path.
    fn capture(&mut self, cp: &Checkpoint) -> PathBuf {
        self.seq.flush().unwrap();
        let mut offset = None;
        for entry in log_entries(&self.dir) {
            offset = Some(crate::log::offset_of(offset, &entry).unwrap());
            if matches!(&entry, LogEntry::Checkpoint(logged) if logged == cp) {
                break;
            }
        }
        let offset = offset.unwrap();
        let context = self.seq.recovery_context(self.seq.snapshot_base.clone().unwrap(), cp);
        let payload = self.seq.snapshot_payload(cp.clone(), self.seq.progress.clone(), context, offset).unwrap();
        let cache = self.cache();
        snapshot::write(&cache, &payload, &self.seq.key).unwrap();
        cache.dir.join(snapshot::name(offset))
    }

    /// Recovers a copy of this node's log, labelled `label`, with the vault finalized at `cp`: from the
    /// cache at `cache` and the log after it, or, with none, by full replay.
    fn recover_copy(&self, cache: Option<&Path>, cp: &Checkpoint, label: &str) -> Node {
        let dir = PathBuf::from(format!("{}-{label}", self.dir.display()));
        copy_dir(&self.dir, &dir);
        let (domain, signer) = (self.domain(), AccountId(self.seq.key.address()));
        let log = match cache {
            Some(path) => {
                let (cached, engine) = snapshot::read(&self.cache(), path, domain.chain_id, domain.vault).unwrap();
                let at = cached.offset;
                let mut replay = Replay::from_snapshot(engine, cached, domain, Some(signer), Some(parent(cp))).unwrap();
                let mut offset = None;
                for entry in log_entries(&dir) {
                    offset = Some(crate::log::offset_of(offset, &entry).unwrap());
                    if offset > Some(at) {
                        replay.apply(&entry).unwrap();
                    }
                }
                let log = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into();
                ReplayedLog {
                    log,
                    replayed: replay.finish(),
                    snapshot_seq: Some(at.seq),
                    legacy_authorizations: Default::default(),
                }
            }
            None => {
                Sequencer::read_log_for_recovery(&dir, Some(domain), Some(signer), Some(parent(cp)), |_| {}).unwrap()
            }
        };
        let (work, queued) = mpsc::channel(64);
        let cfg = SequencerConfig {
            clock: self.clock.clone(),
            checkpoints: Some(work.clone()),
            approval_registry: Some(Default::default()),
            checkpoint_parent: parent(cp),
            ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
        };
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let key = self.seq.key.clone();
        let mut seq = Sequencer::recover_from(log, cfg, key, broadcast::channel(128).0, withdrawals).unwrap();
        seq.trust(sample_around(self.clock.as_ref())).unwrap();
        seq.checkpoint_status(Some(cp.seq), Some(Health::Healthy)).unwrap();
        Node { seq, clock: self.clock.clone(), dir, work, _queued: queued, signed }
    }

    /// Custody's view once the vault finalizes this node's latest checkpoint, at which a recovered
    /// payout is proved, with the trader's approval at `approved` unclaimed.
    fn posted(&mut self, approved: &Checkpoint) -> LifecycleEvidence {
        let latest = self.last_checkpoint();
        self.seq.checkpoint_status(Some(latest.seq), Some(Health::Healthy)).unwrap();
        let mut evidence = self.evidence(&latest);
        evidence.approvals[0].approval.approved_seq = approved.seq;
        evidence
    }
}

/// The finalized anchor `cp` is.
fn parent(cp: &Checkpoint) -> CheckpointParent {
    CheckpointParent { has_checkpoint: true, seq: cp.seq, root: cp.root.parse().unwrap() }
}

/// The trader's authorizations the log in `dir` holds.
fn authorizations(dir: &Path) -> usize {
    log_entries(dir)
        .iter()
        .filter(|entry| matches!(entry, LogEntry::Withdrawal(w) if w.account == TRADER && w.authorization.is_some()))
        .count()
}
