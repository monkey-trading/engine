//! A custody-backed signer history that starts at a daily snapshot's checkpoint, against one from the log's start.

use std::collections::BTreeSet;

use super::fixture::{Fixture, checkpoint_parent};
use crate::checkpoints::{SignerHistory, accepted_history};
use crate::log::{Checkpoint, CheckpointParent, LogEntry};

/// A signer history started at a daily snapshot's checkpoint authenticates what one from the log's start does: the
/// chain from that checkpoint to the vault's anchor, and, read from the vault, a checkpoint before it that a typed
/// payout names with its signer's epoch; with that read missing, a payout naming a checkpoint the vault never
/// accepted is refused.
#[tokio::test]
async fn a_signer_history_from_a_snapshots_checkpoint_is_the_one_from_the_start() {
    let fixture = Fixture::new("audit-history-from-snapshot");
    let (first, anchor) = (fixture.first(), fixture.parent());
    let checkpoints: Vec<Checkpoint> = fixture
        .entries
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint.clone()),
            _ => None,
        })
        .collect();
    let (chain, at, domain) = (&fixture.chain, &fixture.chain.at, fixture.domain);
    let epochs = BTreeSet::from([1, 2]);
    let whole = (anchor, CheckpointParent::default());
    let whole = accepted_history(chain, at, domain, whole, &checkpoints, (epochs.clone(), BTreeSet::new())).await;
    let start = (anchor, checkpoint_parent(&first));
    let earlier = BTreeSet::from([first.seq]);
    let from = accepted_history(chain, at, domain, start, &checkpoints[1..], (epochs.clone(), earlier)).await;
    let (whole, from) = (whole.unwrap(), from.unwrap());
    assert_eq!(from.epochs, whole.epochs, "the signer epochs");
    let seqs =
        |history: &SignerHistory| history.checkpoints.iter().map(|accepted| accepted.seq).collect::<BTreeSet<_>>();
    assert!(seqs(&from).contains(&first.seq), "the checkpoint before the snapshot's, read from the vault");
    assert_eq!(seqs(&from), seqs(&whole), "the checkpoints a payout may name");
    let unknown = BTreeSet::from([first.seq + 1_000]);
    let refused = accepted_history(chain, at, domain, start, &checkpoints[1..], (epochs, unknown)).await;
    assert!(refused.is_err(), "a payout names a checkpoint the vault never accepted");
}
