//! A follower's copy of the operator's log with its live replica. The replica is built once, when the
//! follower starts, by replaying the copy as it is opened, authenticated against the vault's recorded
//! signers; from then it takes every entry the copy takes, before the copy does, and is the authority
//! a start from the copy uses: the files are not read again, so a host that alters them after the
//! build changes nothing the start relies on.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::eip712::B256;
use engine_core::types::AccountId;

use super::chain::{Chain, Step};
use super::follow::{ReplicaRefused, follow};
use super::open_copy_replayed;
use super::operator::Followed;
use crate::config::NodeConfig;
use crate::history::marks::fold::MarkFold;
use crate::log::{CheckpointDomain, CheckpointParent, LogEntry, LogWriter, Offset, offset_of};
use crate::sequencer::ReplayedLog;
use crate::sequencer::replay::LiveReplay;
use crate::stats::Stats;

/// A follower's copy, open for appends, its seal chain, and its replica.
pub(crate) struct Follower {
    writer: LogWriter,
    copied: Chain,
    replica: LiveReplay,
    /// What a member's link answers from: the copy's durable head, latest entries and signed heads.
    link: Option<crate::liveness::link::Copy>,
}

impl Follower {
    /// Opens the copy in `dir` and builds its replica by replaying it as it is read: commands sealed,
    /// checkpoints and typed withdrawals signed, by one of `signers`, the vault's recorded signers,
    /// in `domain`, with `enclave` the followed one. With `stats`, the replica records every command
    /// into it, as recovery would, and `observe` sees each entry as it is read.
    /// Error cases:
    /// - the copy cannot be opened, or does not replay (see `Replay::apply`)
    #[cfg(test)]
    pub(crate) fn open(
        dir: &std::path::Path,
        segment_commands: u64,
        domain: CheckpointDomain,
        enclave: AccountId,
        signers: BTreeSet<AccountId>,
        stats: Option<Stats>,
        mut observe: impl FnMut(&LogEntry),
    ) -> anyhow::Result<Self> {
        let observe = |entry: &LogEntry| {
            observe(entry);
            Ok(())
        };
        Self::open_pinned(dir, segment_commands, domain, enclave, signers, (stats, None), None, observe)
    }

    /// `open`, pinning the posted log head `posted` as the replay passes it (see `LiveReplay::pin`),
    /// so a replica built long after that head still holds it, and with `marks`, folding each mark
    /// the replica applies, from the build on, into it; an `observe` that fails ends the build.
    /// Error cases:
    /// - see `open`, or `observe` fails
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_pinned(
        dir: &std::path::Path,
        segment_commands: u64,
        domain: CheckpointDomain,
        enclave: AccountId,
        signers: BTreeSet<AccountId>,
        (stats, marks): (Option<Stats>, Option<Rc<RefCell<MarkFold>>>),
        posted: Option<(u64, B256)>,
        mut observe: impl FnMut(&LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let fresh = || {
            let mut replica = LiveReplay::new(domain, signers.clone());
            if let Some(stats) = &stats {
                replica.record_stats_with(stats.clone());
            }
            if let Some(marks) = &marks {
                // A retried build folds the same marks again, which leaves every bucket as it was.
                let marks = marks.clone();
                replica
                    .sample_marks_with(Box::new(move |market, ts, mark| marks.borrow_mut().sample(market, ts, mark)));
            }
            replica
        };
        let (writer, copied, replica) =
            open_copy_replayed(dir, segment_commands.max(1), Chain::new(domain, enclave), fresh, |replica, entry| {
                observe(entry)?;
                replica.apply(entry)?;
                if let (Some((seq, link)), LogEntry::Command(command)) = (posted, entry)
                    && command.sequenced.seq == seq
                {
                    replica.pin(seq, link);
                }
                Ok(())
            })
            .context("the copy does not replay")?;
        Ok(Self { writer, copied, replica, link: None })
    }

    /// The replica.
    pub(crate) fn replica(&self) -> &LiveReplay {
        &self.replica
    }

    /// The seq of the copy's last command, and the cumulative prefix after it: the tip a tail
    /// handover names.
    pub(crate) fn tip(&self) -> Option<(u64, B256)> {
        self.replica.last_seq().map(|seq| (seq, self.replica.head_link()))
    }

    /// How many commands the copy's tip trails the operator's head by, as the operator's last signed
    /// heartbeat named it; `None` before a heartbeat, or with an empty copy.
    pub(crate) fn behind(&self) -> Option<u64> {
        Some(self.copied.operator_head()?.seq.saturating_sub(self.replica.last_seq()?))
    }

    /// Keeps the posted head `(seq, link)` as one the replica holds (see `LiveReplay::pin`).
    pub(crate) fn pin(&mut self, seq: u64, link: B256) -> Option<bool> {
        self.replica.pin(seq, link)
    }

    /// Follows `followed` for one stint of up to `budget`, its enclave admitted as one that may seal
    /// the copy's next commands (see `follow`).
    /// Error cases:
    /// - as `follow`; after `ReplicaRefused` the follower must be opened again
    pub(crate) async fn stint(
        &mut self,
        cfg: &NodeConfig,
        followed: &Followed,
        budget: Duration,
    ) -> anyhow::Result<Option<Offset>> {
        self.copied.admit(followed.enclave);
        let link = self.link.as_ref();
        follow(&mut self.writer, &mut self.copied, cfg, followed, budget, Some(&mut self.replica), link).await
    }

    /// Whether a member link records what the copy holds.
    pub(crate) fn linked(&self) -> bool {
        self.link.is_some()
    }

    /// Records what the copy holds into `link` from here on, starting with its head as durable.
    pub(crate) fn link_with(&mut self, link: crate::liveness::link::Copy) {
        if let Some((seq, head)) = self.tip() {
            link.durable(crate::log_heads::Head { seq, link: head });
        }
        self.link = Some(link);
    }

    /// Takes a handover's tail: the JSON lines in `bytes`, the entries the predecessor's log holds
    /// after the command at `after`, the copy's tip. Each entry the copy already holds must repeat
    /// it exactly; each later one must extend the copy's seal chain, and the replica must apply it
    /// before the copy takes it. Returns how many entries the copy took; the copy is synced.
    /// Error cases:
    /// - `after` is not the copy's tip, a line does not parse, an entry repeats the copy's
    ///   differently or does not extend it, or the replica refuses one (`ReplicaRefused`)
    /// - the copy cannot be written or synced
    pub(crate) fn take_tail(&mut self, after: u64, bytes: &[u8]) -> anyhow::Result<u64> {
        ensure!(self.replica.last_seq() == Some(after), "the tail follows seq {after}, not the copy's tip");
        self.copied.begin_after_command();
        let mut previous = Some(Offset::start_of(after));
        let mut taken = 0;
        for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            let entry: LogEntry = serde_json::from_slice(line).context("a tail line is not a log entry")?;
            let offset = offset_of(previous, &entry)?;
            previous = Some(offset);
            if let Step::Extends(link) = self.copied.check(offset, &entry)? {
                self.replica.apply(&entry).map_err(|e| e.context(ReplicaRefused))?;
                self.writer.append(&entry).context("appending a tail entry to the copy")?;
                self.copied.extend(offset, entry, link);
                taken += 1;
            }
        }
        self.writer.sync().context("syncing the copy")?;
        Ok(taken)
    }

    /// Flushes and syncs the copy.
    /// Error cases:
    /// - the write or sync fails
    pub(crate) fn sync(&mut self) -> anyhow::Result<()> {
        self.writer.sync().context("syncing the copy")
    }

    /// The copy as a recovered log, its writer open at its end (see `LiveReplay::into_log`).
    /// Error cases:
    /// - see `LiveReplay::into_log`
    pub(crate) fn into_log(
        self,
        parent: Option<CheckpointParent>,
        signer: AccountId,
        posted: Option<u64>,
    ) -> anyhow::Result<ReplayedLog> {
        self.replica.into_log(self.writer, parent, signer, posted)
    }
}

#[cfg(test)]
mod tests;
