//! A vault that pauses withdrawals or has its signer retired: no first issuance, no reissue and
//! no new withdrawal until finalized custody shows the halt cleared, across a restart.

use super::*;

use crate::checkpoints::Health;

#[derive(Clone, Copy, Debug)]
enum Halt {
    Paused,
    Retired,
}

impl Halt {
    /// `evidence` as the vault shows it under this halt.
    fn apply(self, mut evidence: LifecycleEvidence) -> LifecycleEvidence {
        match self {
            Halt::Paused => evidence.withdrawals_paused = true,
            Halt::Retired => evidence.signer = AccountId([0; 20]),
        }
        evidence
    }

    /// The signer epoch once the timelock clears the halt: lifting a pause keeps the epoch, and
    /// installing a signer, here this enclave's key again, starts the next.
    fn cleared_epoch(self) -> u64 {
        match self {
            Halt::Paused => 1,
            Halt::Retired => 2,
        }
    }
}

#[test]
fn a_halt_stops_first_issuance_and_new_withdrawals_until_finalized_custody_clears_it() {
    for halt in [Halt::Paused, Halt::Retired] {
        let mut node = Node::new(&format!("halt-first-issuance-{halt:?}"));
        let cp = node.approve();
        node.seq.custody_status(Some(cp.seq), Some(Health::Healthy), Some(halt.apply(node.evidence(&cp)))).unwrap();
        // A failed observation leaves the halt in place.
        node.seq.custody_status(None, None, None).unwrap();
        node.seq.flush().unwrap();
        assert!(node.signed.try_recv().is_err(), "{halt:?}: nothing is signed while the vault halts claims");
        assert!(withdrawals(&node.dir).is_empty(), "{halt:?}");
        assert_eq!(node.seq.metrics.withdrawals_paused.get(), 1, "{halt:?}");
        assert!(node.seq.sequence(withdraw(10)).unwrap().is_none(), "{halt:?}: a new withdrawal is refused");

        let mut cleared = node.evidence(&cp);
        cleared.signer_epoch = halt.cleared_epoch();
        node.seq.custody_status(Some(cp.seq), Some(Health::Healthy), Some(cleared)).unwrap();
        node.seq.flush().unwrap();
        let signed = node.signed.try_recv().expect("the cleared vault's first authorization");
        assert_eq!(signed.authorization.unwrap().signer_epoch, halt.cleared_epoch(), "{halt:?}");
        assert_eq!(node.seq.metrics.withdrawals_paused.get(), 0, "{halt:?}");
    }
}

#[test]
fn a_halt_stops_a_requested_reissue_until_finalized_custody_clears_it() {
    for halt in [Halt::Paused, Halt::Retired] {
        let mut node = Node::new(&format!("halt-requested-reissue-{halt:?}"));
        let cp = node.approve();
        let first = released(&mut node, &cp);
        let expires_at = first.authorization.as_ref().unwrap().expires_at;
        node.clock.advance(Duration::from_secs(crate::withdrawals::LIFETIME_SECONDS));
        node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
        assert_eq!(node.seq.request_reissue(TRADER, 1, expires_at, None), Reissue::Queued);

        node.seq.custody_status(Some(cp.seq), Some(Health::Healthy), Some(halt.apply(node.evidence(&cp)))).unwrap();
        node.seq.flush().unwrap();
        assert!(node.signed.try_recv().is_err(), "{halt:?}: a requested reissue waits for the halt to clear");

        let mut cleared = node.evidence(&cp);
        cleared.signer_epoch = halt.cleared_epoch();
        node.seq.custody_status(Some(cp.seq), Some(Health::Healthy), Some(cleared)).unwrap();
        node.seq.flush().unwrap();
        let reissued = node.signed.try_recv().expect("the cleared vault's reissue");
        assert_eq!((reissued.account, reissued.nonce, reissued.amount), (first.account, first.nonce, first.amount));
        assert_eq!(reissued.authorization.unwrap().signer_epoch, halt.cleared_epoch(), "{halt:?}");
        assert!(node.signed.try_recv().is_err(), "{halt:?}: one reissue");
    }
}

#[test]
fn a_rotation_under_a_pause_reissues_a_usable_authorization_only_once_the_pause_ends() {
    let mut node = Node::new("halt-rotation-reissue");
    let cp = node.approve();
    let first = released(&mut node, &cp);
    let mut rotated = Halt::Paused.apply(node.evidence(&cp));
    rotated.signer_epoch = 2;
    node.seq.custody_status(Some(cp.seq), Some(Health::Healthy), Some(rotated.clone())).unwrap();
    node.seq.flush().unwrap();
    assert!(node.signed.try_recv().is_err(), "the new epoch's reissue waits for the pause to end");

    rotated.withdrawals_paused = false;
    node.seq.custody_status(Some(cp.seq), Some(Health::Healthy), Some(rotated)).unwrap();
    node.seq.flush().unwrap();
    let reissued = node.signed.try_recv().expect("the rotation's reissue");
    assert_eq!((reissued.nonce, reissued.authorization.unwrap().signer_epoch), (first.nonce, 2));
    assert!(node.signed.try_recv().is_err(), "one reissue");
}

#[test]
fn a_restarted_sequencer_applies_the_halt_before_any_release() {
    let mut node = Node::new("halt-restart");
    let cp = node.approve();
    node.seq.flush().unwrap();
    // The restart has a healthy verdict but no custody evidence yet, so its due approval waits.
    let mut recovered = node.restart(&cp);
    recovered
        .seq
        .custody_status(Some(cp.seq), Some(Health::Healthy), Some(Halt::Paused.apply(node.evidence(&cp))))
        .unwrap();
    recovered.seq.flush().unwrap();
    assert!(recovered.signed.try_recv().is_err(), "the first evidence after a restart carries the pause");
    assert!(withdrawals(&recovered.dir).is_empty());
    recovered.seq.custody_status(Some(cp.seq), Some(Health::Healthy), Some(node.evidence(&cp))).unwrap();
    recovered.seq.flush().unwrap();
    assert_eq!(recovered.signed.try_recv().unwrap().nonce, 1);
}

#[test]
fn a_retired_signer_withholds_checkpoints_until_the_timelock_installs_one() {
    let mut node = Node::new("halt-withheld-checkpoints");
    let cp = node.approve();
    node.seq
        .custody_status(Some(cp.seq), Some(Health::Healthy), Some(Halt::Retired.apply(node.evidence(&cp))))
        .unwrap();
    node.seq.sequence(Command::Tick).unwrap().unwrap();
    node.seq.checkpoint().unwrap();
    assert_eq!(node.last_checkpoint(), cp, "the vault accepts no checkpoint, so none is signed");
    // A pause alone leaves the vault accepting checkpoints.
    node.seq.custody_status(Some(cp.seq), Some(Health::Healthy), Some(Halt::Paused.apply(node.evidence(&cp)))).unwrap();
    node.seq.checkpoint().unwrap();
    assert!(node.last_checkpoint().seq > cp.seq);
}

#[test]
fn only_a_retired_signer_differs_from_this_enclave_without_failing() {
    let mut node = Node::new("halt-foreign-signer");
    let cp = node.approve();
    let mut foreign = node.evidence(&cp);
    foreign.signer = AccountId([9; 20]);
    let error = node.seq.release_payouts(&foreign).unwrap_err();
    assert!(error.to_string().contains("differs from this enclave"), "{error:#}");
    node.seq.release_payouts(&Halt::Retired.apply(node.evidence(&cp))).unwrap();
    node.seq.flush().unwrap();
    assert!(node.signed.try_recv().is_err());
}

/// Releases the approval's first authorization at epoch 1 and returns it.
fn released(node: &mut Node, cp: &Checkpoint) -> SignedWithdrawal {
    node.seq.custody_status(Some(cp.seq), Some(Health::Healthy), Some(node.evidence(cp))).unwrap();
    node.seq.flush().unwrap();
    node.signed.try_recv().expect("the first authorization")
}

/// The withdrawal authorizations the log holds.
fn withdrawals(dir: &std::path::Path) -> Vec<SignedWithdrawal> {
    log_entries(dir)
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Withdrawal(withdrawal) => Some(withdrawal),
            _ => None,
        })
        .collect()
}
