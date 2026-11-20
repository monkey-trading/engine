//! A promotion is a signer rotation to another key (#595): the promoted member starts from the
//! outgoing key's copy, keeps each unpaid authorization that key signed, and reissues it under its own
//! key and the promotion's epoch once custody reports that epoch.

use super::*;

use crate::sequencer::replay::ReplayedLog;
use crate::standby::Follower;
use crate::test_support::SEGMENT_COMMANDS;

#[test]
fn a_promoted_start_reissues_the_outgoing_keys_unpaid_authorization_under_its_own_key_once() {
    let mut node = Node::new("reissue-promotion");
    let cp = node.approve();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    let issued = node.signed.try_recv().unwrap().authorization.unwrap();
    assert_eq!((issued.signer_epoch, issued.signer), (1, AccountId(node.seq.key.address())));
    let standby = Arc::new(EnclaveKey::generate());
    let mut promoted = node.promote(&cp, standby.clone());
    let payout = &promoted.seq.gate.as_ref().unwrap().payouts[&(TRADER, 1)];
    assert_eq!(payout.released, Some((1, issued.expires_at)), "the outgoing key's authorization is kept");
    let mut rotated = promoted.evidence(&cp);
    rotated.signer_epoch = 2;
    promoted.seq.release_payouts(&rotated).unwrap();
    promoted.seq.flush().unwrap();
    let reissued = promoted.signed.try_recv().unwrap().authorization.unwrap();
    assert_eq!((reissued.signer_epoch, reissued.signer), (2, AccountId(standby.address())));
    promoted.seq.release_payouts(&rotated).unwrap();
    promoted.seq.flush().unwrap();
    assert!(promoted.signed.try_recv().is_err(), "reissued once");
}

/// A promotion reissues nothing before custody's finalised read names the promotion's epoch and shows
/// the nonce unused: a read still at the outgoing epoch, or one that omits the approval, signs nothing.
/// An authorization in flight at the read — its claim not yet final — is reissued under the new epoch,
/// where the vault refuses the outgoing one, and retired once a later read shows it paid.
#[test]
fn a_promoted_start_reissues_only_on_a_finalised_read_of_its_epoch_showing_the_nonce_unused() {
    let mut node = Node::new("reissue-promotion-evidence");
    let cp = node.approve();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    node.signed.try_recv().unwrap();
    let standby = Arc::new(EnclaveKey::generate());
    let mut promoted = node.promote(&cp, standby.clone());
    promoted.seq.release_payouts(&promoted.evidence(&cp)).unwrap();
    promoted.seq.flush().unwrap();
    assert!(promoted.signed.try_recv().is_err(), "custody still reports the outgoing epoch");
    let mut absent = promoted.evidence(&cp);
    absent.signer_epoch = 2;
    absent.approvals.clear();
    promoted.seq.release_payouts(&absent).unwrap();
    promoted.seq.flush().unwrap();
    assert!(promoted.signed.try_recv().is_err(), "absence is not evidence that the nonce is unused");
    let mut rotated = promoted.evidence(&cp);
    rotated.signer_epoch = 2;
    promoted.seq.release_payouts(&rotated).unwrap();
    promoted.seq.flush().unwrap();
    let reissued = promoted.signed.try_recv().unwrap().authorization.unwrap();
    assert_eq!((reissued.signer_epoch, reissued.signer), (2, AccountId(standby.address())));
    rotated.approvals[0].claimed = true;
    promoted.seq.release_payouts(&rotated).unwrap();
    assert!(promoted.seq.gate.as_ref().unwrap().payouts.is_empty(), "paid: retired");
    assert_eq!(promoted.seq.request_reissue(TRADER, 1, reissued.expires_at, None), Reissue::Unknown);
}

/// An authorization custody had already paid when the promotion's read comes is retired, never
/// reissued under the promoted key.
#[test]
fn a_promoted_start_retires_an_authorization_custody_paid_and_reissues_nothing() {
    let mut node = Node::new("reissue-promotion-paid");
    let cp = node.approve();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    node.signed.try_recv().unwrap();
    let mut promoted = node.promote(&cp, Arc::new(EnclaveKey::generate()));
    let mut paid = promoted.evidence(&cp);
    paid.signer_epoch = 2;
    paid.approvals[0].claimed = true;
    promoted.seq.release_payouts(&paid).unwrap();
    promoted.seq.flush().unwrap();
    assert!(promoted.signed.try_recv().is_err(), "a paid nonce is never signed again");
    assert!(promoted.seq.gate.as_ref().unwrap().payouts.is_empty());
}

/// An authorization the outgoing key signed that expired before the promotion waits for its account's
/// screened request, as today, and that request is signed under the promoted key and epoch.
#[test]
fn a_promoted_start_leaves_an_expired_authorization_for_its_accounts_request() {
    let mut node = Node::new("reissue-promotion-expired");
    let cp = node.approve();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    let issued = node.signed.try_recv().unwrap().authorization.unwrap();
    node.clock.advance(Duration::from_secs(crate::withdrawals::LIFETIME_SECONDS));
    let standby = Arc::new(EnclaveKey::generate());
    let mut promoted = node.promote(&cp, standby.clone());
    let mut rotated = promoted.evidence(&cp);
    rotated.signer_epoch = 2;
    promoted.seq.release_payouts(&rotated).unwrap();
    promoted.seq.flush().unwrap();
    assert!(promoted.signed.try_recv().is_err(), "an expired authorization is not reissued automatically");
    assert_eq!(promoted.seq.request_reissue(TRADER, 1, issued.expires_at, None), Reissue::Queued);
    promoted.seq.release_payouts(&rotated).unwrap();
    promoted.seq.flush().unwrap();
    let requested = promoted.signed.try_recv().unwrap().authorization.unwrap();
    assert_eq!((requested.signer_epoch, requested.signer), (2, AccountId(standby.address())));
    assert!(requested.expires_at > issued.expires_at);
}

/// A promotion can fix a checkpoint the vault accepted whose entry no member received: the promoted
/// start proves an approval only at a checkpoint its log holds, so the authorization it signs names a
/// root that a replay of its log checks.
#[test]
fn a_promoted_start_proves_approvals_only_at_a_checkpoint_its_log_holds() {
    let mut node = Node::new("reissue-promotion-unheld");
    let cp = node.approve();
    let copy = scratch_dir("reissue-promotion-unheld-copy");
    let (manifest, _) = watch::channel(Manifest::default());
    let mut writer =
        crate::log::SegmentedLog::open(&copy, |_| Ok(())).unwrap().into_writer(SEGMENT_COMMANDS, manifest).unwrap();
    for entry in log_entries(&node.dir) {
        if !matches!(&entry, LogEntry::Checkpoint(checkpoint) if checkpoint.seq == cp.seq) {
            writer.append(&entry).unwrap();
        }
    }
    writer.sync().unwrap();
    drop(writer);
    let standby = Arc::new(EnclaveKey::generate());
    let mut promoted = node.promote_from(&copy, &cp, standby.clone());
    let payout = &promoted.seq.gate.as_ref().unwrap().payouts[&(TRADER, 1)];
    assert!(payout.proof.is_none(), "proved at the fixed checkpoint, which the log lacks");
    promoted.seq.sequence(Command::Tick).unwrap().expect("admitted");
    promoted.seq.checkpoint().unwrap();
    promoted.seq.flush().unwrap();
    let held = promoted.last_checkpoint();
    assert!(held.seq > cp.seq, "the promoted key's own checkpoint");
    promoted.seq.checkpoint_status(Some(held.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    let mut rotated = promoted.evidence(&held);
    rotated.signer_epoch = 2;
    rotated.approvals[0].approval.approved_seq = cp.seq;
    promoted.seq.release_payouts(&rotated).unwrap();
    promoted.seq.flush().unwrap();
    let reissued = promoted.signed.try_recv().unwrap().authorization.unwrap();
    assert_eq!((reissued.checkpoint_seq, reissued.signer), (held.seq, AccountId(standby.address())));
    let attested = [AccountId(node.seq.key.address()), AccountId(standby.address())].into();
    crate::sequencer::replay_path_with_attested(&copy, Some(node.domain()), attested, Default::default())
        .expect("a replay of the promoted log checks every authorization's root");
}

impl Node {
    /// The node `standby` starts as by a promotion that fixed the vault's checkpoint `cp`: from a
    /// copy of this node's log, as a member's replica finishes it under the promoted key.
    fn promote(&self, cp: &Checkpoint, standby: Arc<EnclaveKey>) -> Node {
        self.promote_from(&self.dir, cp, standby)
    }

    /// `promote` from the copy in `dir`, which may lack `cp`'s entry: its state at the tip anchors there.
    fn promote_from(&self, dir: &std::path::Path, cp: &Checkpoint, standby: Arc<EnclaveKey>) -> Node {
        let outgoing = AccountId(self.seq.key.address());
        let mut follower =
            Follower::open(dir, SEGMENT_COMMANDS, self.domain(), outgoing, [outgoing].into(), None, |_| {}).unwrap();
        if follower.replica().last_seq() == Some(cp.seq) {
            follower.capture_tip();
        }
        let parent = crate::log::CheckpointParent { has_checkpoint: true, seq: cp.seq, root: cp.root.parse().unwrap() };
        let log: ReplayedLog = follower.into_promoted_log(Some(parent), AccountId(standby.address()), None).unwrap();
        let cfg = SequencerConfig { checkpoint_parent: parent, ..self.config() };
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let mut seq = Sequencer::recover_from(log, cfg, standby, broadcast::channel(128).0, withdrawals).unwrap();
        seq.trust(sample_around(self.clock.as_ref())).unwrap();
        seq.checkpoint_status(Some(cp.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
        let (work, queued) = mpsc::channel(64);
        Node { seq, clock: self.clock.clone(), dir: dir.to_path_buf(), work, _queued: queued, signed }
    }
}
