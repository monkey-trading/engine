//! A history index following a `TestSequencer`'s log.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::api::{Feed, MAX_BACKFILLS};
use crate::config::HistorySection;
use crate::history::Reader;
use crate::history::derived::Deriver;
use crate::log::Offset;
use crate::test_support::{RETENTION, TestSequencer, open_history};

/// A history index following a `TestSequencer`'s log: its reader, where it resumed, and its task.
pub struct TestHistory {
    pub reader: Reader,
    pub resume_at: Offset,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestHistory {
    /// Opens the index in `index` against the log `node` writes in `dir`, and runs it, letting
    /// readers in once it has taken the log's last command now.
    /// Error cases:
    /// - the index cannot be opened, nor a fresh one in its place (see `HistoryCheck`)
    pub async fn start(node: &TestSequencer, dir: &Path, index: &Path) -> anyhow::Result<Self> {
        let tip = node.next_seq().await.checked_sub(1);
        Self::start_with_tip(node, dir, index, tip)
    }

    /// As `start`, but readers are let in once the index has taken `tip`.
    /// Error cases:
    /// - the index cannot be opened, nor a fresh one in its place (see `HistoryCheck`)
    pub fn start_with_tip(node: &TestSequencer, dir: &Path, index: &Path, tip: Option<u64>) -> anyhow::Result<Self> {
        let cfg = HistorySection { dir: index.to_owned(), retention: RETENTION, marks: None };
        let mut history = open_history(&cfg, dir)?;
        history.catch_up_to(tip);
        let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
        let (reader, resume_at) = (history.reader(), history.resume_at());
        let feed =
            Feed::new(node.stream.subscribe(), dir.to_owned(), node.manifest.clone(), backfills, Some(resume_at));
        let deriver = Deriver::new(node.derived.clone(), Some((dir.to_owned(), node.manifest.clone())));
        Ok(Self { reader, resume_at, task: tokio::spawn(history.run(feed, deriver)) })
    }

    /// The task's result once it has taken the whole log, which it ends with after the sequencer
    /// stops.
    /// Error cases:
    /// - the history task failed (see `History::run`)
    pub async fn finish(self) -> anyhow::Result<()> {
        timeout(Duration::from_secs(10), self.task).await.expect("the history task ends in time").expect("history task")
    }
}
