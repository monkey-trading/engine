//! A custody-backed signer history that starts at a daily snapshot's checkpoint, against one from the log's start.

use std::collections::BTreeSet;

use super::fixture::{Fixture, checkpoint_parent};
use crate::checkpoints::{SignerHistory, accepted_history};
use crate::log::{Checkpoint, CheckpointParent, LogEntry};

/// A signer history started at a daily snapshot's checkpoint authenticates what one from the log's start does: the
/// chain from that checkpoint to the vault's anchor, the epochs of the earlier signers the snapshot carries, and, read
/// from the vault, a checkpoint before it that a typed payout names with its signer's epoch; a payout naming a
/// checkpoint the vault never accepted is refused.
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
    let none = BTreeSet::new();
    let whole = (anchor, CheckpointParent::default(), &none);
    let whole = accepted_history(chain, at, domain, whole, &checkpoints, (epochs.clone(), BTreeSet::new())).await;
    let prior = BTreeSet::from([first.signer.parse().unwrap()]);
    let start = (anchor, checkpoint_parent(&first), &prior);
    let earlier = BTreeSet::from([first.seq]);
    let from = accepted_history(chain, at, domain, start, &checkpoints[1..], (epochs.clone(), earlier)).await;
    let (whole, from) = (whole.unwrap(), from.unwrap());
    assert_eq!(from.epochs, whole.epochs, "the signer epochs");
    let seqs =
        |history: &SignerHistory| history.checkpoints.iter().map(|accepted| accepted.seq).collect::<BTreeSet<_>>();
    assert!(seqs(&from).contains(&first.seq), "the checkpoint before the snapshot's, read from the vault");
    assert_eq!(seqs(&from), seqs(&whole), "the checkpoints a payout may name");
    // With no payout naming an epoch, the earlier signer the snapshot carries still brings its epoch in, as the
    // log's earlier checkpoints do for a history from the log's start.
    let bare = accepted_history(chain, at, domain, start, &checkpoints[1..], (BTreeSet::new(), BTreeSet::new())).await;
    assert_eq!(bare.unwrap().epochs, whole.epochs, "the earlier signer's epoch");
    let unknown = BTreeSet::from([first.seq + 1_000]);
    let refused = accepted_history(chain, at, domain, start, &checkpoints[1..], (epochs, unknown)).await;
    assert!(refused.is_err(), "a payout names a checkpoint the vault never accepted");
}
