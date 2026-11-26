//! What a daily snapshot carries of the history the node derives from the log (`History`): the dashboard record at
//! its checkpoint (`stats::Record`), and the marks cache's committed chain anchors there, as the frozen copy taken
//! at it holds the cache, with the root over that copy's rows (`Frozen`). The snapshot's signature covers both
//! (`Covered::history`), so a start from the snapshot restores the record and takes the copy on its signer's word,
//! under whatever key the start holds, and reads no log before the snapshot for either. A part the node holds none of
//! is left out: a node with no marks task, as a member, freezes no copy.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tracing::warn;

use crate::history::marks::store::Frozen;
use crate::stats::{Capture, Record};

/// The history a daily snapshot carries.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct History {
    /// The dashboard record at the checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<Record>,
    /// The marks cache's committed chain anchors at the checkpoint, as its frozen copy holds it, and the root over the
    /// copy's rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marks: Option<Frozen>,
}

/// The history a daily snapshot will carry, as the sequencer takes it at the checkpoint: the record, which the
/// scans in flight then complete, and the end the marks task hands the anchors to once it has frozen the cache.
pub(crate) struct Pending {
    stats: Option<Capture>,
    marks: Option<Receiver<Frozen>>,
}

impl Pending {
    /// The history of `stats`, the record taken at the checkpoint, and of the anchors `marks` gives.
    pub(crate) fn new(stats: Option<Capture>, marks: Option<Receiver<Frozen>>) -> Self {
        Self { stats, marks }
    }

    /// No history, which encodes as `{}`: for a test of a read that does not look at it.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self { stats: None, marks: None }
    }

    /// The history's encoding, each part once it is complete, both within `within`: a part that does not come by
    /// then is left out and logged, and a start from the snapshot goes without it.
    /// Error cases:
    /// - the history cannot be encoded
    pub(crate) fn wait(self, within: Duration) -> anyhow::Result<String> {
        let deadline = Instant::now() + within;
        let stats = self.stats.and_then(|capture| {
            let record = capture.wait(within);
            if record.is_none() {
                warn!("the dashboard record at the daily snapshot's checkpoint did not complete, so it carries none");
            }
            record
        });
        let marks = self.marks.and_then(|frozen| {
            let anchors = frozen.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok();
            if anchors.is_none() {
                warn!("the marks task froze no copy of its cache for the daily snapshot, so it carries no anchors");
            }
            anchors
        });
        serde_json::to_string(&History { stats, marks }).context("encoding the history")
    }
}

impl History {
    /// The history `raw`, a daily snapshot's, holds; none for a snapshot that carries none, or one this build
    /// cannot read, which is logged, and a start goes without it.
    pub(crate) fn read(raw: Option<&RawValue>) -> Self {
        let Some(raw) = raw else { return Self::default() };
        serde_json::from_str(raw.get())
            .inspect_err(|e| warn!("the daily snapshot's history does not decode, so a start goes without it: {e:#}"))
            .unwrap_or_default()
    }
}
