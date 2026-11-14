//! The gate on payouts while a turn whose set names two or more members is in effect: a check comes
//! due once this node holds a turn on that its verified head does not cover, a signature waits for a
//! member's verification of the log through its approval, one from a release that sequenced the
//! range is refused, a reissue waits as a first signature does, a one-member set or rotation off
//! leaves #14's gate alone, and a restart restores the verified head only when the log holds its
//! checkpoint.

use super::*;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use engine_core::Fixed;
use engine_core::auth::Operator;
use engine_core::command::Command;
use engine_core::emissions::TokenUnits;
use engine_core::turns::SetMember;
use engine_core::types::AccountId;
use tokio::sync::{broadcast, mpsc};

use crate::checkpoints::{ApprovalId, ApprovalStatus, Health, LifecycleEvidence};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::log::{Checkpoint, LogEntry, Manifest};
use crate::sequencer::fixture_tests::finished_genesis;
use crate::sequencer::tests::operator::turn_log_under;
use crate::sequencer::{Reissue, ReissueScreening, SequencerConfig, SignedWithdrawal};
use crate::tee::EnclaveKey;
use crate::tee::keys::KeySet;
use crate::tee::sealing::MockSealer;
use crate::test_support::{access, dev_config, log_entries, sample_around, scratch_dir, sequencer_config};

const DIGEST: [u8; 48] = [0x44; 48];
const CONFIG: [u8; 32] = [0xc1; 32];
const OTHER: B256 = B256::repeat_byte(0xb2);

#[test]
fn a_held_turn_in_a_two_member_set_signs_only_what_a_members_check_verified() {
    let mut node = Node::start("log-check-held", &[other()]);
    node.approve(1);
    assert!(node.due.borrow().is_none(), "a check came due before any turn was in effect");
    let (turn, taken_seq) = node.take_turn();

    // The check comes due at the checkpoint the turn's `TurnTaken` is followed by.
    let due = node.due.borrow().clone().expect("no check came due");
    let head = due.head;
    assert!(head.seq >= taken_seq, "the check's checkpoint {} precedes the TurnTaken at {taken_seq}", head.seq);
    assert_eq!((due.turn, due.excluded.clone()), (turn, BTreeSet::from([node.release])));
    assert_eq!(head, node.checkpoint_head(head.seq));

    // Posted and healthy, the approval still waits for the check, and the payouts read names its boundary.
    node.release();
    assert!(node.signed.try_recv().is_err(), "an approval was signed before a member verified the log through it");
    let boundary = engine_core::turns::boundary(turn).unwrap();
    assert_eq!(node.check_of(1), Some(CheckWait { turn, boundary_ms: boundary }));

    // A verification from this release, which sequenced the range, or of another head, is refused.
    assert!(!node.seq.log_checked(head, node.release), "a check from a release in the range was taken");
    assert!(!node.seq.log_checked(VerifiedHead { root: OTHER, ..head }, OTHER), "a check of another head was taken");
    node.release();
    assert!(node.signed.try_recv().is_err(), "a refused check released a signature");

    // Another member's verification is taken, sealed, and releases the approval.
    assert!(node.seq.log_checked(head, OTHER));
    assert!(node.due.borrow().is_none(), "the check stayed due once verified");
    assert_eq!(node.store.keys().log_check, Some(head), "the verified head was not sealed");
    node.release();
    assert_eq!(node.signed.try_recv().expect("the verified approval was not signed").nonce, 1);
    assert_eq!(node.check_of(1), None);

    // An approval after the verified head waits for the next boundary's check.
    node.approve(2);
    node.release();
    assert!(node.signed.try_recv().is_err(), "an approval past the verified head was signed");
    let next = engine_core::turns::boundary(turn + 1).unwrap();
    assert_eq!(node.check_of(2), Some(CheckWait { turn: turn + 1, boundary_ms: next }));
}

#[test]
fn a_one_member_set_leaves_payouts_to_the_posted_root_alone() {
    let mut node = Node::start("log-check-one-member", &[]);
    node.approve(1);
    node.take_turn();
    assert!(node.due.borrow().is_none(), "a one-member set asked for a check");
    assert_eq!(node.check_of(1), None);
    node.release();
    assert_eq!(node.signed.try_recv().expect("#14's gate alone signs it").nonce, 1);
}

#[test]
fn a_restart_restores_the_verified_head_only_when_the_log_holds_its_checkpoint() {
    let mut node = Node::start("log-check-restart", &[other()]);
    node.approve(1);
    node.take_turn();
    let (head, turn) = (node.due.borrow().clone().expect("no check came due").head, node.turn);
    assert!(node.seq.log_checked(head, OTHER));
    let restarted = node.restart();
    assert!(
        restarted.seq.log_check.covers(&restarted.seq.engine, head.seq),
        "the sealed verified head was not restored"
    );
    assert!(restarted.due.borrow().is_none());

    // A sealed head the log does not hold, as a host restoring another log's file gives, is dropped,
    // and the check is due again.
    restarted.store.update(|keys| keys.log_check = Some(VerifiedHead { root: OTHER, ..head })).unwrap();
    let mut again = restarted.restart();
    assert!(!again.seq.log_check.covers(&again.seq.engine, head.seq), "a head the log does not hold was restored");
    again.seq.refresh_log_check();
    assert_eq!(again.due.borrow().as_ref().map(|due| due.turn), Some(turn), "the check is not due again");
}

#[test]
fn a_verified_head_at_the_command_before_the_turn_leaves_no_check_due() {
    // A handover's check: the successor verified the log through the checkpoint before its TurnTaken.
    let mut node = Node::start("log-check-handed-over", &[other()]);
    node.approve(1);
    node.seq.checkpoint().unwrap();
    let last = node.seq.engine.last_seq().unwrap();
    let handed = node.checkpoint_head(last);
    node.store.update(|keys| keys.log_check = Some(handed)).unwrap();
    let mut successor = node.restart();
    successor.take_turn();
    assert!(successor.due.borrow().is_none(), "a check came due over the handover's");
    assert!(successor.seq.log_check.covers(&successor.seq.engine, last));
}

#[test]
fn rotation_off_leaves_payouts_to_the_posted_root_alone() {
    let mut node = Node::start("log-check-rotation-off", &[other()]);
    node.approve(1);
    node.take_turn();
    assert!(node.due.borrow().is_some(), "no check came due with rotation on");
    let off =
        Command::SwitchRotation { enabled: false, election_cancelled: false, block: 14, chain_ref: "0x04:0".into() };
    let events = node.seq.sequence(off).unwrap().expect("admitted");
    assert!(!events.iter().any(|event| matches!(event, Event::ElectionRejected { .. })), "{events:?}");
    assert!(node.seq.engine.daily_turns().in_effect.is_some(), "the drawn turn stands");
    node.seq.refresh_log_check();
    assert!(node.due.borrow().is_none(), "a check stayed due with rotation off");
    assert_eq!(node.check_of(1), None);
    node.release();
    assert_eq!(node.signed.try_recv().expect("#14's gate alone signs it").nonce, 1);
}

#[test]
fn a_reissue_on_a_signer_change_waits_for_the_check_as_a_first_signature_does() {
    let mut node = Node::start("log-check-reissue-epoch", &[other()]);
    node.approve(1);
    // Signed before any turn is in effect, under #14's gate alone.
    node.release();
    assert_eq!(node.signed.try_recv().expect("signed before the turn").nonce, 1);
    node.take_turn();
    let head = node.due.borrow().clone().expect("no check came due").head;

    node.release_at(2);
    assert!(node.signed.try_recv().is_err(), "a signer change reissued an approval no check covers");
    assert!(node.seq.log_checked(head, OTHER));
    node.release_at(2);
    let reissued = node.signed.try_recv().expect("the covered approval was not reissued");
    assert_eq!((reissued.nonce, reissued.authorization.unwrap().signer_epoch), (1, 2));
}

#[test]
fn a_reissue_its_holder_requests_waits_for_the_check_as_a_first_signature_does() {
    let mut node = Node::start("log-check-reissue-request", &[other()]);
    node.approve(1);
    node.release();
    let first = node.signed.try_recv().expect("signed before the turn");
    let expires_at = first.authorization.expect("a typed authorization").expires_at;
    node.take_turn();
    let head = node.due.borrow().clone().expect("no check came due").head;

    // The authorization expires unclaimed and its holder asks for another.
    let now = node.clock.now_ms() / 1_000;
    node.clock.advance(Duration::from_secs(expires_at.saturating_sub(now as u64)));
    node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
    // The screening is observed at the engine's time.
    node.seq.sequence(Command::Tick).unwrap().expect("admitted");
    let screening = node.reissue_screening(1, expires_at);
    assert_eq!(node.seq.request_reissue(trader(), 1, expires_at, Some(&screening)), Reissue::Queued);
    node.release();
    assert!(node.signed.try_recv().is_err(), "a requested reissue signed an approval no check covers");
    assert!(node.seq.log_checked(head, OTHER));
    node.release();
    assert_eq!(node.signed.try_recv().expect("the covered approval was not reissued").nonce, 1);
}

#[test]
fn the_target_is_the_first_checkpoint_at_or_after_the_turn_taken_whichever_is_noted_first() {
    let taken = |release| [Event::TurnTaken { turn: 1, candidate: AccountId([1; 20]), release }];
    // The schedule writes a checkpoint at the `TurnTaken` before the `TurnTaken` is recorded.
    let mut check = LogCheck::default();
    check.command(0, &[], None);
    check.checkpointed(head_at(17));
    check.checkpointed(head_at(23));
    check.command(23, &taken(OTHER), Some(OTHER));
    assert_eq!(check.target, Some(head_at(23)));
    check.checkpointed(head_at(30));
    assert_eq!(check.target, Some(head_at(23)), "a chosen target moved");

    // The `TurnTaken` is recorded first, and the checkpoint after it is the target.
    let mut check = LogCheck::default();
    check.command(0, &[], None);
    check.checkpointed(head_at(17));
    check.command(23, &taken(OTHER), Some(OTHER));
    assert!(check.target.is_none_or(|target| target.seq < 23), "a checkpoint before the TurnTaken was chosen");
    check.checkpointed(head_at(24));
    assert_eq!(check.target, Some(head_at(24)));
}

// ── Helpers ──

/// A node whose key the log assigns a turn an hour away, in a set naming it and `others`, with the
/// checkpoint gate, sealed keys and a log check.
struct Node {
    seq: Sequencer,
    clock: Arc<TestClock>,
    dir: PathBuf,
    turn: u64,
    member: AccountId,
    store: Arc<KeyStore>,
    release: B256,
    due: watch::Receiver<Option<Due>>,
    signed: mpsc::UnboundedReceiver<SignedWithdrawal>,
    _queued: mpsc::Receiver<crate::checkpoints::Work>,
}

impl Node {
    /// The node over a fresh turn log for `name`.
    fn start(name: &str, others: &[SetMember]) -> Self {
        let (dir, clock, turn, member) = turn_log_under(name, others, Some(access::policy()));
        let sealer = MockSealer { launch_digest: DIGEST };
        let keys = scratch_dir(&format!("{name}-keys"));
        let store = Arc::new(KeyStore::create(&keys, &sealer, CONFIG, 1, KeySet::generate().unwrap()).unwrap());
        Self::recover(dir, clock, (turn, member), store)
    }

    /// The node over `dir`'s log with the keys `store` holds, as a restart recovers it.
    fn recover(dir: PathBuf, clock: Arc<TestClock>, (turn, member): (u64, AccountId), store: Arc<KeyStore>) -> Self {
        let release = B256::from(store.release().id());
        let (due_tx, due) = watch::channel(None);
        let (work, queued) = mpsc::channel(64);
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let operator = Operator { address: member, ..dev_config().operator().unwrap() };
        let setup = LogCheckSetup { release, store: store.clone(), due: due_tx };
        let cfg = SequencerConfig {
            clock: clock.clone(),
            genesis: finished_genesis(&log_entries(&dir)),
            operator,
            turn_release: Some(release),
            log_check: Some(setup),
            access_policy: Some(access::policy()),
            checkpoints: Some(work),
            approval_registry: Some(Default::default()),
            ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
        };
        let key = Arc::new(EnclaveKey::test_key());
        let mut seq = Sequencer::recover(cfg, key, broadcast::channel(16).0, withdrawals).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        seq.checkpoint_status(seq.last_seq(), Some(Health::Healthy)).unwrap();
        seq.refresh_log_check();
        Self { seq, clock, dir, turn, member, store, release, due, signed, _queued: queued }
    }

    /// The same node recovered from its log and sealed keys.
    fn restart(self) -> Self {
        let Node { seq, clock, dir, turn, member, store, .. } = self;
        drop(seq);
        Self::recover(dir, clock, (turn, member), store)
    }

    /// Credits the trader and approves its withdrawal `nonce`, with a checkpoint that covers it, each
    /// screened by the test access source.
    fn approve(&mut self, nonce: u64) {
        if nonce == 1 {
            self.seq.sequence(crate::test_support::empty_custody_frontier()).unwrap();
            let declared = self.seq.sequence(access::declaration(&self.seq.engine, SEED)).unwrap().expect("admitted");
            assert!(matches!(declared.as_slice(), [Event::EligibilityAttested { .. }]), "{declared:?}");
        }
        let reference = format!("0xlog-check-funding:{nonce}");
        let funding =
            access::funding(&self.seq.engine, Some(trader()), Some(trader()), Fixed::from_int(100), &reference);
        self.seq.sequence(funding).unwrap().expect("admitted");
        let withdraw = access::withdrawal(&self.seq.engine, SEED, Fixed::from_int(40), true);
        let events = self.seq.sequence(withdraw).unwrap().expect("admitted");
        let approved = |event: &Event| matches!(event, Event::WithdrawalApproved { nonce: n, .. } if *n == nonce);
        assert!(events.iter().any(approved), "{events:?}");
        self.seq.checkpoint().unwrap();
    }

    /// Moves past the turn's boundary and sequences its `TurnTaken`: the turn and its seq.
    fn take_turn(&mut self) -> (u64, u64) {
        // The vault holds the last checkpoint, so the one the check needs is not held back behind it.
        let last = self.last_checkpoint();
        self.seq.checkpoint_status(Some(last.seq), Some(Health::Healthy)).unwrap();
        let begins = engine_core::turns::boundary(self.turn).unwrap();
        self.clock.advance(Duration::from_millis((begins + 1_000 - self.clock.now_ms()) as u64));
        self.seq.trust(sample_around(self.clock.as_ref())).unwrap();
        self.seq.sequence(Command::Tick).unwrap().expect("admitted");
        let taken = self.seq.engine.daily_turns().taken.expect("the turn was taken");
        assert_eq!((taken.turn, taken.release), (self.turn, self.release));
        let (seq, _) = self.seq.log_check.takings.last().expect("the TurnTaken was recorded");
        (self.turn, seq)
    }

    /// Releases payouts on finalized evidence that the vault holds the latest checkpoint and every
    /// approval is unpaid, then flushes, so custody sees what was signed.
    fn release(&mut self) {
        self.release_at(1);
    }

    /// `release`, with the vault in signer epoch `signer_epoch`.
    fn release_at(&mut self, signer_epoch: u64) {
        let approvals = self.seq.gate.as_ref().unwrap().payouts.values().map(|payout| ApprovalStatus {
            approval: ApprovalId {
                account: payout.approval.account,
                nonce: payout.approval.nonce,
                approved_seq: payout.approval.approved_seq,
            },
            claimed: false,
        });
        let latest = self.last_checkpoint();
        let evidence = LifecycleEvidence {
            height: 10,
            block_hash: B256::repeat_byte(1),
            signer: AccountId(self.seq.key.address()),
            signer_epoch,
            withdrawals_paused: false,
            total_withdrawn: Fixed::ZERO,
            checkpoint_seq: Some(latest.seq),
            checkpoint_root: Some(latest.root),
            approvals: approvals.collect(),
        };
        self.seq.checkpoint_status(evidence.checkpoint_seq, Some(Health::Healthy)).unwrap();
        self.seq.release_payouts(&evidence).unwrap();
        self.seq.flush().unwrap();
    }

    /// The test access source's clear screening of the trader's request to reissue approval `nonce`,
    /// replacing the authorization that expires at `replaces`.
    fn reissue_screening(&self, nonce: u64, replaces: u64) -> ReissueScreening {
        let domain = self.seq.engine.access_domain().unwrap();
        let request_hash = crate::api::reissue::reissue_hash(&domain, trader(), nonce, replaces);
        let evidence = access::proof(&self.seq.engine, request_hash, Some(trader()), None);
        ReissueScreening { request_hash, evidence }
    }

    /// The boundary the trader's approval `nonce` waits for, as the payouts read says.
    fn check_of(&self, nonce: u64) -> Option<CheckWait> {
        let page = self.seq.account_payouts(trader(), None, 10);
        page.payouts.into_iter().find(|payout| payout.nonce == nonce).expect("the approval is unpaid").check
    }

    /// The checkpoint the log holds at `seq`, with the prefix after its command.
    fn checkpoint_head(&self, seq: u64) -> VerifiedHead {
        let checkpoint = log_entries(&self.dir)
            .into_iter()
            .find_map(|entry| match entry {
                LogEntry::Checkpoint(checkpoint) if checkpoint.seq == seq => Some(checkpoint),
                _ => None,
            })
            .expect("a checkpoint at the seq");
        let link = self.seq.prefixes.prefix_at(seq).expect("the prefix is retained");
        VerifiedHead { seq, link, root: checkpoint.root.parse().unwrap() }
    }

    /// The last checkpoint the log holds.
    fn last_checkpoint(&self) -> Checkpoint {
        log_entries(&self.dir)
            .into_iter()
            .rev()
            .find_map(|entry| match entry {
                LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
                _ => None,
            })
            .expect("a checkpoint")
    }
}

/// The withdrawing account's seed in the test access source.
const SEED: u8 = 1;

/// The withdrawing account.
fn trader() -> AccountId {
    access::account(SEED)
}

/// A second member of the set.
fn other() -> SetMember {
    SetMember { candidate: AccountId([0xc2; 20]), admin_key: AccountId([0xd2; 20]), weight: TokenUnits(1) }
}

/// A checkpoint at `seq`.
fn head_at(seq: u64) -> VerifiedHead {
    VerifiedHead { seq, link: B256::repeat_byte(seq as u8), root: B256::repeat_byte(!(seq as u8)) }
}
