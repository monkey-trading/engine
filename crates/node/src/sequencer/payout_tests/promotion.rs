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

impl Node {
    /// The node `standby` starts as by a promotion that fixed the vault's checkpoint `cp`: from a
    /// copy of this node's log, as a member's replica finishes it under the promoted key.
    fn promote(&self, cp: &Checkpoint, standby: Arc<EnclaveKey>) -> Node {
        let outgoing = AccountId(self.seq.key.address());
        let follower =
            Follower::open(&self.dir, SEGMENT_COMMANDS, self.domain(), outgoing, [outgoing].into(), None, |_| {})
                .unwrap();
        let parent = crate::log::CheckpointParent { has_checkpoint: true, seq: cp.seq, root: cp.root.parse().unwrap() };
        let log: ReplayedLog = follower.into_promoted_log(Some(parent), AccountId(standby.address()), None).unwrap();
        let cfg = SequencerConfig { checkpoint_parent: parent, ..self.config() };
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let mut seq = Sequencer::recover_from(log, cfg, standby, broadcast::channel(128).0, withdrawals).unwrap();
        seq.trust(sample_around(self.clock.as_ref())).unwrap();
        seq.checkpoint_status(Some(cp.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
        let (work, queued) = mpsc::channel(64);
        Node { seq, clock: self.clock.clone(), dir: self.dir.clone(), work, _queued: queued, signed }
    }
}
