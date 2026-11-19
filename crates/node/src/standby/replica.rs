//! A follower's copy of the operator's log with its live replica. The replica is built once, when the
//! follower starts, by replaying the copy as it is opened, authenticated against the vault's recorded
//! signers; from then it takes every entry the copy takes, before the copy does, and is the authority
//! a start from the copy uses: the files are not read again, so a host that alters them after the
//! build changes nothing the start relies on.

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use tracing::error;

use super::chain::{Chain, Step, rollback_of};
use super::follow::{ReplicaRefused, follow, follow_until};
use super::open_copy_replayed;
use super::operator::Followed;
use crate::checkpoints::Takeover;
use crate::config::NodeConfig;
use crate::history::derived::RowsStore;
use crate::history::marks::fold::MarkFold;
use crate::log::{CheckpointDomain, CheckpointParent, LogEntry, LogWriter, Manifest, Offset, offset_of};
use crate::log_check::answer::CheckedHeads;
use crate::log_heads::Head;
use crate::sequencer::ReplayedLog;
use crate::sequencer::replay::LiveReplay;
use crate::stats::Stats;

/// How long `follow_during` waits before following again after a stint the operator ended.
const FOLLOW_PAUSE: Duration = Duration::from_millis(100);

/// A follower's copy, open for appends, its seal chain, and its replica.
pub(crate) struct Follower {
    writer: LogWriter,
    copied: Chain,
    replica: LiveReplay,
    /// What a member's link answers from: the copy's durable head, latest entries and signed heads.
    link: Option<crate::liveness::link::Copy>,
    /// The newest entries the copy held when it was opened, with their offsets, for the member link
    /// to serve once one links the copy.
    opened: VecDeque<(Offset, LogEntry)>,
    /// The copy's directory and the manifest its writer publishes, which the member link reads the copy
    /// from beyond the entries it keeps.
    dir: std::path::PathBuf,
    manifest: tokio::sync::watch::Receiver<Manifest>,
    /// The signers the vault recorded, whose seals a log taken from its first entry may carry before any
    /// checkpoint of the copy names them.
    signers: BTreeSet<AccountId>,
    /// How many whole replays of the copy its open ran: two when it cut an unaccepted tail.
    replays: u64,
}

/// What a replica records as it applies entries: the dashboard record, the folded marks, the checkpoints it
/// verified and the store of each command's derived rows.
pub(crate) type Records = (Option<Stats>, Option<Rc<RefCell<MarkFold>>>, Option<CheckedHeads>, Option<Arc<RowsStore>>);

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
        Self::open_pinned(
            dir,
            segment_commands,
            domain,
            enclave,
            signers,
            (stats, None, None, None),
            (None, &[]),
            observe,
        )
    }

    /// `open`, pinning the posted log head `posted` as the replay passes it (see `LiveReplay::pin`),
    /// so a replica built long after that head still holds it, keeping the links and checkpoints at
    /// the seqs `keep` names (see `LiveReplay::keep`), with `marks`, folding each mark the replica
    /// applies, from the build on, into it, and with `checked`, recording each checkpoint it verifies
    /// there (`LiveReplay::record_checked_with`), and with `rows`, the store each command's derived rows
    /// are written to as the replica applies them (`LiveReplay::store_rows_in`); an `observe` that fails
    /// ends the build. A tail after the copy's last checkpoint that no accepted signer sealed is copied
    /// aside to `<dir>.unaccepted-<seq>`, paged as Critical `copy_tail_unaccepted`, and cut back (`cut`),
    /// and the replica rebuilt.
    /// Error cases:
    /// - see `open`, or `observe` fails
    /// - the copy holds such a tail and no checkpoint before it, or the tail cannot be kept or cut
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_pinned(
        dir: &std::path::Path,
        segment_commands: u64,
        domain: CheckpointDomain,
        enclave: AccountId,
        signers: BTreeSet<AccountId>,
        (stats, marks, checked, rows): Records,
        (posted, keep): (Option<(u64, B256)>, &[u64]),
        mut observe: impl FnMut(&LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let recorded = signers.clone();
        let fresh = || {
            let mut replica = LiveReplay::new(domain, signers.clone());
            keep.iter().for_each(|seq| replica.keep(*seq));
            if let Some(stats) = &stats {
                replica.record_stats_with(stats.clone());
            }
            if let Some(checked) = &checked {
                replica.record_checked_with(checked.clone());
            }
            if let Some(rows) = &rows {
                replica.store_rows_in(rows.clone());
            }
            if let Some(marks) = &marks {
                // Each build folds the copy's marks from its start, and none of a tail an earlier one dropped.
                marks.borrow_mut().restart();
                let marks = marks.clone();
                replica
                    .sample_marks_with(Box::new(move |market, ts, mark| marks.borrow_mut().sample(market, ts, mark)));
            }
            replica
        };
        let (mut opened, mut previous) = (VecDeque::new(), None);
        let mut apply = |replica: &mut LiveReplay, entry: &LogEntry| {
            observe(entry)?;
            replica.apply(entry)?;
            let offset = offset_of(previous, entry)?;
            // A retried build reads the copy again from its start.
            if previous.is_some_and(|was| offset <= was) {
                opened.clear();
            }
            previous = Some(offset);
            opened.push_back((offset, entry.clone()));
            if opened.len() > crate::liveness::link::RECENT_ENTRIES {
                opened.pop_front();
            }
            if let (Some((seq, link)), LogEntry::Command(command)) = (posted, entry)
                && command.sequenced.seq == seq
            {
                replica.pin(seq, link);
            }
            Ok(())
        };
        let chain = Chain::new(domain, enclave);
        let opened_copy = open_copy_replayed(dir, segment_commands.max(1), chain.clone(), fresh, &mut apply)
            .context("the copy does not replay")?;
        // A tail no accepted signer sealed is not the operator's history to vouch for: a reorged
        // promotion's, or one a host or an archive forged. It is kept aside as evidence and cut back to
        // the last checkpoint, which an accepted signer's seals reach, and the stream refills it.
        let mut replays = 1;
        let (writer, mut copied, replica) = match opened_copy.2.unaccepted_from() {
            None => opened_copy,
            Some((seq, sealer)) => {
                let replica = &opened_copy.2;
                let checkpoint = replica.last_checkpoint().cloned().with_context(|| {
                    format!("the copy's commands from seq {seq} carry no seal by an accepted signer, and no checkpoint precedes them")
                })?;
                let last = replica.last_seq().unwrap_or(seq);
                drop(opened_copy);
                let kept = super::cut::keep_tail(dir, seq).context("keeping the copy's unaccepted tail")?;
                error!(
                    alert = "copy_tail_unaccepted",
                    key = %sealer.map_or_else(|| "none".into(), |key| key.to_string()),
                    seq,
                    "the copy's command at seq {seq} carries no seal by a key its replay accepts, and no \
                     checkpoint covers it or the commands after it to seq {last}: a promotion a reorg removed, or a \
                     tail an archive mirror or the host forged; they are kept at {} and the copy is cut back to its \
                     checkpoint at seq {}, for the stream to refill",
                    kept.display(),
                    checkpoint.seq
                );
                let signer = checkpoint.signer.parse().context("the checkpoint's signer")?;
                let to = Takeover { seq: checkpoint.seq, root: checkpoint.root, signer };
                super::cut::cut_marked(dir, &to).context("cutting the copy's unaccepted tail")?;
                replays += 1;
                open_copy_replayed(dir, segment_commands.max(1), chain, fresh, &mut apply)
                    .context("the copy does not replay")?
            }
        };
        // The stream may carry commands a recorded signer sealed that no checkpoint the copy holds names,
        // as a promoted epoch's before its first, which a cut, now or at an earlier open, dropped: the
        // stream's refill takes them, as a tail does.
        recorded.iter().for_each(|signer| copied.admit(*signer));
        let (published, manifest) = tokio::sync::watch::channel(Manifest::default());
        let mut writer = writer;
        writer.publish_on(published);
        let dir = dir.to_owned();
        Ok(Self { writer, copied, replica, link: None, opened, dir, manifest, signers: recorded, replays })
    }

    /// Drops the entries the copy held when it was opened, once the copy may take others the link has not
    /// recorded: a link given them would serve a ring with a hole in it.
    fn forget_opened(&mut self) {
        if self.link.is_none() {
            self.opened.clear();
        }
    }

    /// Keeps the replica's link and checkpoint at `seq` past its retention windows once taken (see
    /// `LiveReplay::keep`).
    pub(crate) fn keep(&mut self, seq: u64) {
        self.replica.keep(seq);
    }

    /// How many whole replays of the copy its open ran.
    pub(crate) fn replays(&self) -> u64 {
        self.replays
    }

    /// The replica.
    pub(crate) fn replica(&self) -> &LiveReplay {
        &self.replica
    }

    /// Records the replica's anchor work at its tip when the copy holds no checkpoint entry there (see
    /// `LiveReplay::capture_tip`).
    pub(crate) fn capture_tip(&mut self) {
        self.replica.capture_tip();
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

    /// Whether the last stint repeated the copy's whole tail, or extended an empty copy: the operator's
    /// log holds the copy's history. A stint that only heard a heartbeat has not.
    pub(crate) fn confirmed(&self) -> bool {
        self.copied.confirmed()
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
        // The followed enclave is the vault's signer at the latest block: a promotion's standby key
        // signs from its promotion's block, before the set this replica was built with names it.
        self.copied.admit(followed.enclave);
        self.replica.admit(followed.enclave);
        self.forget_opened();
        let link = self.link.as_ref();
        follow(&mut self.writer, &mut self.copied, cfg, followed, budget, Some(&mut self.replica), link).await
    }

    /// Follows `followed` while `work` runs, a stint at a time for at most `budget` in all, so what the
    /// operator seals meanwhile reaches the copy and its member link: an incumbent whose handover's
    /// final checkpoint waits for a member to confirm its seq can take this member's confirmation.
    /// Returns `work`'s output, and why the following stopped early: the replica refused an entry
    /// (`ReplicaRefused`, after which the follower must be opened again), or the operator's log no
    /// longer extends the copy; or, when no stint reached the operator, the last failure, the
    /// connecting that `work`'s end cut short included.
    pub(crate) async fn follow_during<T>(
        &mut self,
        cfg: &NodeConfig,
        followed: &Followed,
        budget: Duration,
        work: impl Future<Output = T>,
    ) -> (T, anyhow::Result<()>) {
        self.forget_opened();
        let (ended, until) = tokio::sync::watch::channel(false);
        let work = async move {
            let output = work.await;
            ended.send_replace(true);
            output
        };
        let following = async {
            let started = tokio::time::Instant::now();
            // The last failure, kept while no stint has reached the operator.
            let (mut reached, mut failure) = (false, None);
            loop {
                let left = budget.saturating_sub(started.elapsed());
                if *until.borrow() || left.is_zero() {
                    return match failure.filter(|_| !reached) {
                        Some(e) => Err(e),
                        None => Ok(()),
                    };
                }
                self.copied.admit(followed.enclave);
                self.replica.admit(followed.enclave);
                let (stint, link) = ((left, Some(until.clone())), self.link.as_ref());
                match follow_until(
                    &mut self.writer,
                    &mut self.copied,
                    cfg,
                    followed,
                    stint,
                    Some(&mut self.replica),
                    link,
                )
                .await
                {
                    Err(e) if e.downcast_ref::<ReplicaRefused>().is_some() || rollback_of(&e).is_some() => {
                        return Err(e);
                    }
                    Err(e) => {
                        tracing::debug!("following while waiting failed, following again: {e:#}");
                        failure = Some(e);
                    }
                    Ok(_) => reached = true,
                }
                // A stream the operator ends at once must not make this a spin.
                let mut paused = until.clone();
                tokio::select! {
                    () = tokio::time::sleep(FOLLOW_PAUSE) => {}
                    _ = paused.wait_for(|ended| *ended) => {}
                }
            }
        };
        tokio::join!(work, following)
    }

    /// The offset of the copy's last entry, `None` for an empty copy.
    pub(crate) fn head(&self) -> Option<Offset> {
        self.copied.head()
    }

    /// Whether a member link records what the copy holds.
    pub(crate) fn linked(&self) -> bool {
        self.link.is_some()
    }

    /// Records what the copy holds into `link` from here on, starting with the links the replica
    /// retains, its head as durable, and the entries it held when it was opened, to serve.
    pub(crate) fn link_with(&mut self, link: crate::liveness::link::Copy) {
        link.rebuilt();
        link.logged(self.dir.clone(), self.manifest.clone());
        link.served(std::mem::take(&mut self.opened));
        for retained in self.replica.retained_links() {
            link.linked(retained.seq, retained.link, retained.sealer);
        }
        if let Some((seq, head)) = self.tip() {
            link.durable(crate::log_heads::Head { seq, link: head }, self.replica.tip_sealer());
        }
        self.link = Some(link);
    }

    /// Takes a tail: the JSON lines in `bytes`, the entries another log holds after the entry at
    /// `after`, the copy's tip command or an entry past it at that seq. Each entry the copy already
    /// holds must repeat it (`Chain::check`); each later one must extend the copy's seal chain, and the replica
    /// must apply it before the copy takes it. Returns how many entries the copy took; the copy is
    /// synced.
    /// Error cases:
    /// - `after` is not at the copy's tip, a line does not parse, an entry repeats the copy's
    ///   differently or does not extend it, or the replica refuses one (`ReplicaRefused`)
    /// - the copy cannot be written or synced
    pub(crate) fn take_tail(&mut self, after: Offset, bytes: &[u8]) -> anyhow::Result<u64> {
        self.take_entries(Some(after), bytes)
    }

    /// `take_tail`, or for `None` the whole log onto an empty copy, from its first command.
    /// Error cases:
    /// - as `take_tail`, or `None` for a copy that holds a command
    pub(crate) fn take_entries(&mut self, after: Option<Offset>, bytes: &[u8]) -> anyhow::Result<u64> {
        match after {
            Some(after) => {
                ensure!(self.replica.last_seq() == Some(after.seq), "the tail follows {after}, not the copy's tip");
                self.copied.begin_after(after);
            }
            None => ensure!(self.replica.last_seq().is_none(), "the entries start the log, but the copy holds some"),
        }
        // A page may be sealed by a key an earlier signer change replaced, before the checkpoint that names
        // it: a copy opened again between pages has lost that admission.
        self.signers.iter().for_each(|signer| self.copied.admit(*signer));
        let mut previous = after;
        let (mut taken, mut last) = (0, None);
        for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            let entry: LogEntry = serde_json::from_slice(line).context("a tail line is not a log entry")?;
            let offset = offset_of(previous, &entry)?;
            previous = Some(offset);
            if let Step::Extends(link) = self.copied.check(offset, &entry)? {
                self.replica.apply(&entry).map_err(|e| e.context(ReplicaRefused))?;
                self.writer.append(&entry).context("appending a tail entry to the copy")?;
                match &self.link {
                    Some(link) => {
                        link.appended(offset, &entry);
                        if let LogEntry::Command(command) = &entry {
                            link.linked(
                                command.sequenced.seq,
                                self.replica.head_link(),
                                command.seal.as_ref().map(|seal| seal.signer),
                            );
                            last = Some((
                                Head { seq: command.sequenced.seq, link: self.replica.head_link() },
                                command.seal.as_ref().map(|seal| seal.signer),
                            ));
                        }
                    }
                    None => self.forget_opened(),
                }
                self.copied.extend(offset, entry, link);
                taken += 1;
            }
        }
        self.writer.sync().context("syncing the copy")?;
        if let (Some(link), Some((head, sealer))) = (&self.link, last) {
            link.durable(head, sealer);
        }
        Ok(taken)
    }

    /// Flushes and syncs the copy.
    /// Error cases:
    /// - the write or sync fails
    pub(crate) fn sync(&mut self) -> anyhow::Result<()> {
        self.writer.sync().context("syncing the copy")
    }

    /// The copy as a recovered log under the standby key a promotion installed (see
    /// `LiveReplay::into_promoted_log`).
    /// Error cases:
    /// - see `LiveReplay::into_promoted_log`
    pub(crate) fn into_promoted_log(
        self,
        parent: Option<CheckpointParent>,
        signer: AccountId,
        posted: Option<u64>,
    ) -> anyhow::Result<ReplayedLog> {
        self.replica.into_promoted_log(self.writer, parent, signer, posted)
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
