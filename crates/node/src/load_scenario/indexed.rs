//! The history index of the launch configuration, as the soak's node runs it beside the log's window: unpaired, with
//! no `[snapshots]`, in `history.dir` with the release configuration's 30-day retention shortened with the soak's day
//! (`Archived::retention`). It is planned as `node::recovery` plans it: opened read-only, matched against the log
//! the host holds, and resumed after its last command, or rebuilt from where the host's log and its daily snapshots
//! let it (`archive::history_start`), which the log's window keeps the segments for (`archive::window::Floors`).

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::broadcast;

use crate::config::HistorySection;
use crate::history::HistoryPlan;
use crate::load_scenario::durable::{Durable, Pending};
use crate::log::Manifest;
use crate::sequencer::StreamMsg;

/// The history index of `durable`'s run, planned against the log in `dir`, whose last command is `last_seq`, and
/// taking `stream`. It prints where it resumes and the seq its rebuild would start from (`history_start`), the floor
/// the log's window holds for it.
/// Error cases:
/// - the index cannot be opened, or the segments a rebuild takes cannot be put back from the archive
pub(super) async fn open(
    durable: &Durable,
    dir: &Path,
    last_seq: Option<u64>,
    stream: &broadcast::Sender<Arc<StreamMsg>>,
) -> anyhow::Result<Pending> {
    let cfg = &durable.cfg;
    let mut plan = HistoryPlan::open(&cfg.history).context("opening the history index")?;
    if let Some(last) = last_seq {
        plan.observe_before(dir, last);
    }
    let view = Manifest::load(dir).context("the log's manifest")?;
    let held = plan.last_seq();
    let start = crate::archive::history_start(cfg, &view, |from| plan.resumes_from(from)).await?;
    let history = plan.fresh_from(start).finish().context("resuming the history index")?;
    println!(
        "SOAK history index opened: history_start={start:?} index_last_seq_before={held:?} resumes_at={} log_last_seq={last_seq:?}",
        history.resume_at()
    );
    let section = HistorySection { dir: cfg.history.dir.clone(), retention: cfg.history.retention, marks: None };
    Ok(Pending::unpaired(history, stream.subscribe(), section))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::clock::now_ms;
    use crate::common::launch_book::{NamedCounts, build};
    use crate::history::INDEX_FILE;
    use crate::load_scenario::archived::Archived;
    use crate::load_scenario::node::{self, Boot, Node};
    use crate::load_scenario::{book_mix, moves, venues, wait_priced};
    use crate::sequencer::recovery_scale::Base;
    use crate::test_support::scratch_dir;

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn the_launch_shape_runs_the_unpaired_index_and_reports_its_lag() {
        let mut book = build(&book_mix(now_ms(), 200), NamedCounts { adders: 1, openers: 2, closeouts: 0 });
        // The log starts from the book at seq 0, as the soak's does.
        let base = Base::new(&std::mem::take(&mut book.h.engine)).expect("the book encodes");
        drop(book);
        let venues = venues::start(moves(), Duration::from_millis(20), false).await;
        let run = scratch_dir("indexed");
        let day = Duration::from_secs(120);
        let durable = Durable::new(&run, Duration::from_secs(300)).launching(&run, Archived::start(day).await);
        let (listener, _) = node::listen().await;
        let dir = durable.cfg.node.log_dir.clone();
        let node = Node::start(Boot::Book(Box::new(base.engine())), &dir, listener, &venues, Some(&durable)).await;
        wait_priced(&node.tx).await;
        assert!(node.history_runs(), "the node runs its history index");
        assert!(node.history_behind().await.is_some(), "the index's lag against the log is readable");
        assert!(durable.snapshots().is_none(), "the index runs with no [snapshots]");
        node.stop(true).await.expect("the node and its index stop");
        let last = HistoryPlan::open(&durable.cfg.history).expect("the index opens").last_seq();
        assert!(last.is_some(), "the index in {} holds the log's commands", durable.cfg.history.dir.display());
        assert!(durable.cfg.history.dir.join(INDEX_FILE).exists());
    }
}
