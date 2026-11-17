//! A reorganisation's receipt reversals hold signing until the next finalised custody frontier, which
//! the custody adapter logs only once its scan has no orphan left: a root between two of them would
//! name, as a gap the exit refunds, the id a credited deposit moved to, or an id past the vault's own
//! deposits, which refuses the exit outright.

use std::path::Path;

use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate};
use engine_core::eip712::B256;
use engine_core::event::Event;

use super::*;
use crate::alerts::Status;
use crate::checkpoints::Work;
use crate::sequencer::SequencerConfig;

#[test]
fn no_checkpoint_is_signed_between_a_reorganisations_transitions() {
    // Credited deposits at ids 0 and 1; the reorganisation orphans id 0 and either reincludes id 1's
    // transaction at id 0 or orphans it too.
    for reincluded in [true, false] {
        let dir = scratch_dir("reorg-transitions-withheld");
        let (mut seq, mut queued) = gated(&dir);
        let (orphan, other) = (receipt(0, 40), receipt(1, 100));
        credit(&mut seq, &orphan);
        credit(&mut seq, &other);
        assert!(drain(&mut queued) > 0, "credits are signed while no receipt is reversed");
        let lines = crate::alerts::capture(|| {
            reverse(&mut seq, &orphan);
            assert_eq!(drain(&mut queued), 0, "a root here names id 0 a gap below a live id 1");
            if reincluded {
                let replacement =
                    DepositReceipt { block: 12, block_hash: B256::repeat_byte(0xc0), deposit_id: 0, ..other.clone() };
                let observed_at_seq = revision(&seq, &other);
                let update = DepositUpdate::Replace {
                    receipt: other.clone(),
                    observed_at_seq,
                    replacement: replacement.clone(),
                    replacement_reversed_at_seq: None,
                };
                seq.sequence(Command::CustodyDeposit(Box::new(update))).unwrap();
                assert_eq!(status(&seq, &replacement), DepositStatus::Unfinalised);
            } else {
                reverse(&mut seq, &other);
            }
            seq.sequence(Command::Tick).unwrap();
            assert!(!frontier(&mut seq, 4), "a frontier behind the last is refused");
            assert_eq!(drain(&mut queued), 0, "only an applied frontier ends the hold");
            assert!(frontier(&mut seq, 6));
        });
        assert!(drain(&mut queued) > 0, "the frontier's own checkpoint is signed");
        let key = "checkpoint_withheld{cause=custody_prefix}".to_owned();
        let pages: Vec<_> =
            crate::alerts::pages_for(lines).into_iter().filter(|(_, page)| page.starts_with("checkpoint_")).collect();
        assert_eq!(pages, [(Status::Firing, key.clone()), (Status::Resolved, key)], "reincluded: {reincluded}");
    }
}

#[test]
fn a_restart_between_a_reorganisations_transitions_signs_nothing_until_the_next_frontier() {
    let dir = scratch_dir("reorg-restart-withheld");
    let (mut seq, queued) = gated(&dir);
    let (orphan, other) = (receipt(0, 40), receipt(1, 100));
    credit(&mut seq, &orphan);
    credit(&mut seq, &other);
    reverse(&mut seq, &orphan);
    seq.flush().unwrap();
    drop((seq, queued));
    let (sink, mut queued) = mpsc::channel(128);
    let mut seq = recover(&dir, sink);
    seq.trust(trusted_now()).unwrap();
    seq.sequence(Command::Tick).unwrap();
    assert_eq!(drain(&mut queued), 0, "the replayed reversal still holds signing");
    reverse(&mut seq, &other);
    assert_eq!(drain(&mut queued), 0);
    assert!(frontier(&mut seq, 6));
    assert!(drain(&mut queued) > 0, "the frontier ends the hold after a restart too");
}

// ── Helpers ──

/// A gated sequencer at `V23` in `dir` that checkpoints every command, past its genesis and a first
/// finalised custody frontier, with its checkpoints so far drained.
fn gated(dir: &Path) -> (Sequencer, mpsc::Receiver<Work>) {
    let (sink, mut queued) = mpsc::channel(128);
    let mut seq = recover(dir, sink);
    seq.activate_protocol_at(engine_core::protocol::ProtocolVersion::V23).unwrap();
    seq.genesis(&genesis()).unwrap();
    seq.trust(trusted_now()).unwrap();
    assert!(frontier(&mut seq, 5));
    drain(&mut queued);
    (seq, queued)
}

fn recover(dir: &Path, sink: mpsc::Sender<Work>) -> Sequencer {
    let cfg = SequencerConfig {
        checkpoints: Some(sink),
        checkpoint_every: 1,
        genesis: crate::sequencer::genesis_commands(&genesis()),
        ..sequencer_config(dir, watch::channel(Manifest::default()).0)
    };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(128).0, mpsc::unbounded_channel().0)
        .unwrap()
}

fn genesis() -> crate::config::Genesis {
    let mut genesis = dev_config().genesis;
    genesis.house_capital = Fixed::ZERO;
    genesis
}

/// The trader's deposit of `amount` USDC at vault id `id`, in a block the reorganisation abandons.
fn receipt(id: u8, amount: i64) -> DepositReceipt {
    DepositReceipt {
        block: 10 + u64::from(id),
        block_hash: B256::repeat_byte(0xa0 + id),
        tx_hash: B256::repeat_byte(0xb0 + id),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: u64::from(id),
        beneficiary: DepositBeneficiary::Trader { account: crate::test_support::TRADER },
        amount: Fixed::from_int(amount),
    }
}

fn credit(seq: &mut Sequencer, receipt: &DepositReceipt) {
    seq.sequence(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: receipt.clone() }))).unwrap();
    assert_eq!(status(seq, receipt), DepositStatus::Unfinalised);
}

fn reverse(seq: &mut Sequencer, receipt: &DepositReceipt) {
    let update = DepositUpdate::Reverse { receipt: receipt.clone(), observed_at_seq: revision(seq, receipt) };
    seq.sequence(Command::CustodyDeposit(Box::new(update))).unwrap();
    assert_eq!(status(seq, receipt), DepositStatus::Reversed);
}

/// Sequences the finalised custody frontier at `block`, below every receipt here, and returns
/// whether the engine applied it.
fn frontier(seq: &mut Sequencer, block: u64) -> bool {
    let frontier = CustodyCheckpoint { block, block_hash: B256::repeat_byte(0xf0), deposit_count: 0 };
    let events = seq.sequence(Command::CustodyFinalised(frontier)).unwrap().expect("trusted time stamps it");
    !events.iter().any(|event| matches!(event, Event::AdminRejected { .. }))
}

fn revision(seq: &Sequencer, receipt: &DepositReceipt) -> u64 {
    seq.engine.recorded_deposit(&receipt.reference()).expect("recorded receipt").updated_at_seq
}

fn status(seq: &Sequencer, receipt: &DepositReceipt) -> DepositStatus {
    seq.engine.recorded_deposit(&receipt.reference()).expect("recorded receipt").status.clone()
}

/// How many checkpoints were signed since the last drain.
fn drain(queued: &mut mpsc::Receiver<Work>) -> usize {
    std::iter::from_fn(|| queued.try_recv().ok()).count()
}
