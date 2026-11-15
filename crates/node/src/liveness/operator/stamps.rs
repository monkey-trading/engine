//! The released heads' stamps the rule matches confirmations against, sampled as the sequencer
//! releases them rather than at the rule's polls: a round that waits on its reads or its members
//! must not move the start of the log's unconfirmed run, which the stamp bound counts from, later.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::liveness::Released;

/// How many released heads' stamps the operator keeps for the confirmations it matches: at one
/// sample each `SAMPLE_EVERY`, more than the longest heartbeat interval the schedule allows.
const STAMPS: usize = 16_384;
/// The least time between two samples: the run's start is read at most this much late.
const SAMPLE_EVERY: Duration = Duration::from_millis(20);

/// The stamps kept, by seq, and the samples taken since they were last read.
pub(super) struct Stamps {
    kept: BTreeMap<u64, Released>,
    sampled: mpsc::UnboundedReceiver<Released>,
}

impl Stamps {
    /// Stamps sampled from `released` by the returned task, which runs until `released` closes or
    /// the stamps are dropped.
    pub(super) fn sample(mut released: watch::Receiver<Released>) -> (Self, tokio::task::JoinHandle<()>) {
        let (sender, sampled) = mpsc::unbounded_channel();
        let sampling = tokio::spawn(async move {
            while released.changed().await.is_ok() {
                if sender.send(*released.borrow_and_update()).is_err() {
                    return;
                }
                tokio::time::sleep(SAMPLE_EVERY).await;
            }
        });
        (Self { kept: BTreeMap::new(), sampled }, sampling)
    }

    /// Takes the samples since the last call and `tip`, keeping the newest `STAMPS`, and returns the
    /// stamps kept.
    pub(super) fn with(&mut self, tip: Released) -> &BTreeMap<u64, Released> {
        while let Ok(sampled) = self.sampled.try_recv() {
            self.kept.insert(sampled.head.seq, sampled);
        }
        self.kept.insert(tip.head.seq, tip);
        while self.kept.len() > STAMPS {
            self.kept.pop_first();
        }
        &self.kept
    }
}
