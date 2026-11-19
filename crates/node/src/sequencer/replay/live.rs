//! A replay kept live: a follower's replica applies every entry its copy takes, so a start from it
//! finishes the replay the recovery of the same log would have produced, without reading the log
//! again. It retains what that finish needs: each recent checkpoint's custody work, which anchors the
//! checkpoint gate at the vault's finalized checkpoint, and the latest commands' cumulative prefixes,
//! the evidence that the replica holds a posted log head or a predecessor's tip.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use anyhow::{Context, ensure};
use engine_core::eip712::B256;
use engine_core::types::{AccountId, Millis};
use tracing::warn;

use crate::checkpoints::Work;
use crate::history::derived::RowsStore;
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent, LogEntry, LogWriter};
use crate::sequencer::replay::{Replay, ReplayedLog};
use crate::stats::Stats;

/// How many checkpoints' custody work a replica retains: the vault's finalized checkpoint must be
/// among them for a start from the replica, which otherwise recovers from the log on disk.
const RETAINED_CHECKPOINTS: usize = 4096;
/// How many of the latest commands' cumulative prefixes a replica retains.
pub(crate) const RETAINED_LINKS: usize = if cfg!(test) { 256 } else { 1 << 16 };

/// A command's cumulative command-prefix digest, its sequenced time, and the key that sealed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Link {
    pub seq: u64,
    pub link: B256,
    pub ts: Millis,
    pub sealer: Option<AccountId>,
}

/// A replay of a follower's copy that takes each entry the copy takes.
pub(crate) struct LiveReplay {
    replay: Replay,
    /// Each retained checkpoint's custody work, and the stamp of the command it covers, by seq.
    checkpoints: BTreeMap<u64, (Work, Millis)>,
    /// Whether every checkpoint since the log's start is retained.
    complete: bool,
    /// The latest commands' links, oldest first, at consecutive seqs ending at the last command.
    links: VecDeque<Link>,
    /// The newest posted log head the replica held when it passed, kept past `links`.
    pinned: Option<Link>,
    /// Where a member's replica records each checkpoint it verifies, for its log check answers.
    checked: Option<crate::log_check::answer::CheckedHeads>,
    /// The seqs whose command link and checkpoint the replica keeps past `links` and `checkpoints`
    /// as it passes them (a promotion's fixed head and checkpoint), with what it kept.
    keeping: BTreeSet<u64>,
    kept: BTreeMap<u64, Link>,
    kept_checkpoints: BTreeMap<u64, Checkpoint>,
    /// Where each command's derived rows are written as the replica applies it (`store_rows_in`).
    rows: Option<Arc<RowsStore>>,
}

impl LiveReplay {
    /// An empty replica of a log in `domain`, accepting checkpoints, typed withdrawals and command
    /// seals from the `attested` signers: the vault's recorded signers, as a follower spans every
    /// signer epoch the log does. The copy is another node's log, so every command a checkpoint covers
    /// must carry a seal by one of them (`Replay::copy`); a follower cuts back a tail no checkpoint covers
    /// that holds another seal or none (`Follower::open_pinned`).
    pub(crate) fn new(domain: CheckpointDomain, attested: BTreeSet<AccountId>) -> Self {
        Self {
            replay: Replay::copy(domain, attested),
            checkpoints: BTreeMap::new(),
            complete: true,
            links: VecDeque::new(),
            pinned: None,
            checked: None,
            keeping: BTreeSet::new(),
            kept: BTreeMap::new(),
            kept_checkpoints: BTreeMap::new(),
            rows: None,
        }
    }

    /// `new`, starting from `engine`, a state no log holds (`Sequencer::take_over`), where a replica
    /// of a whole log starts from an empty one.
    #[cfg(test)]
    pub(crate) fn seeded(domain: CheckpointDomain, attested: BTreeSet<AccountId>, engine: engine_core::Engine) -> Self {
        let mut live = Self::new(domain, attested);
        live.replay.engine = engine;
        live
    }

    /// Has the replica write the derived rows of each command it applies to `store`, the history's, so an
    /// index that resumes where a turn left it finds them there once the replica's memory has dropped them
    /// (`Deriver`), not only the newest `RETAINED` commands'. A file already holding a command's rows is
    /// left as it is, so a build over a copy whose files are in place writes none. A write is not synced:
    /// the replica's replay derives the rows again. A failed write is paged and leaves the
    /// replica as it was: the index replays the log for those rows instead.
    pub(crate) fn store_rows_in(&mut self, store: Arc<RowsStore>) {
        self.rows = Some(store);
    }

    /// Has the replica record each checkpoint it verifies, with the turn assignment taken there, into
    /// `checked`.
    pub(crate) fn record_checked_with(&mut self, checked: crate::log_check::answer::CheckedHeads) {
        self.checked = Some(checked);
    }

    /// Keeps the command link and the checkpoint at `seq` past the retention windows, once replayed.
    pub(crate) fn keep(&mut self, seq: u64) {
        self.keeping.insert(seq);
    }

    /// Drops every retained link and checkpoint, as a replay long past them leaves the windows.
    #[cfg(test)]
    pub(crate) fn forget_retained(&mut self) {
        self.links.clear();
        self.checkpoints.clear();
        self.pinned = None;
    }

    /// Has the replica record every command it applies into `stats`, which starts over.
    pub(crate) fn record_stats_with(&mut self, stats: Stats) {
        self.replay.record_stats_with(stats);
    }

    /// Has the replica hand each market's mark, as it changes, to `sink`, as recovery's replay
    /// seeds the marks cache.
    pub(crate) fn sample_marks_with(&mut self, sink: crate::history::marks::MarkSink) {
        self.replay.sample_marks_with(sink);
    }

    /// Applies the copy's next entry, as recovery's replay checks it (see `Replay::apply`).
    /// Error cases:
    /// - the entry fails replay; the replica may then hold part of it and must be rebuilt
    pub(crate) fn apply(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        // A command sealed by another key than the last: a promotion's first, where the copy may hold
        // no entry for the vault's checkpoint at the last command.
        if let LogEntry::Command(command) = entry
            && let Some(sealer) = command.seal.as_ref().map(|seal| seal.signer)
            && self.replay.last_sealer().is_some_and(|last| last != sealer)
        {
            self.capture_tip();
        }
        self.replay.apply(entry)?;
        match entry {
            LogEntry::Command(command) => {
                if let Some(store) = &self.rows
                    && let Some(rows) = self.replay.derived_at(command.sequenced.seq)
                    && !store.holds(command.sequenced.seq, command.sequenced.ts, &rows)
                    && let Err(e) = store.record_unsynced(command.sequenced.seq, command.sequenced.ts, &rows)
                {
                    warn!(
                        alert = "financing_rows_unstored",
                        seq = command.sequenced.seq,
                        "the financing read cannot prove these rows: {e:#}"
                    );
                }
                let link = Link {
                    seq: command.sequenced.seq,
                    link: B256::from(self.replay.command_prefix),
                    ts: command.sequenced.ts,
                    sealer: command.seal.as_ref().map(|seal| seal.signer),
                };
                self.links.push_back(link);
                if self.keeping.contains(&link.seq) {
                    self.kept.insert(link.seq, link);
                }
                if self.links.len() > RETAINED_LINKS {
                    self.links.pop_front();
                }
            }
            LogEntry::Checkpoint(checkpoint) => {
                if self.keeping.contains(&checkpoint.seq) {
                    self.kept_checkpoints.entry(checkpoint.seq).or_insert_with(|| checkpoint.clone());
                }
                let engine = &self.replay.engine;
                let work = Work::capture(checkpoint.clone(), self.replay.accounting, engine);
                // A second checkpoint at one seq extends no outstanding chain in recovery's replay
                // either: the first stays.
                self.checkpoints.entry(checkpoint.seq).or_insert((work, engine.now()));
                if let Some(checked) = &self.checked {
                    let link = B256::from(self.replay.command_prefix);
                    let head =
                        crate::log_check::VerifiedHead { seq: checkpoint.seq, link, root: checkpoint.root.parse()? };
                    let taken = engine.daily_turns().taken.map(|taken| (taken.turn, taken.position));
                    // The replay checked the checkpoint's signature against its signer above.
                    let signer = checkpoint.signer.parse()?;
                    checked.record(crate::log_check::answer::Checked { head, taken, signer });
                }
                if self.checkpoints.len() > RETAINED_CHECKPOINTS {
                    self.checkpoints.pop_first();
                    self.complete = false;
                }
            }
            LogEntry::Withdrawal(_) => {}
        }
        Ok(())
    }

    /// The engine as the entries applied so far leave it.
    pub(crate) fn engine(&self) -> &engine_core::Engine {
        &self.replay.engine
    }

    /// The `TurnTaken` commands and mirrored reassignments the entries applied so far record.
    pub(crate) fn takings(&self) -> &crate::log_check::Takings {
        &self.replay.takings
    }

    /// The state root of the entries applied so far.
    #[cfg(test)]
    pub(crate) fn state_root(&mut self) -> [u8; 32] {
        self.replay.engine.state_root()
    }

    /// The seq of the last command applied.
    pub(crate) fn last_seq(&self) -> Option<u64> {
        self.replay.engine.last_seq()
    }

    /// The first command of the replica's tail that no accepted signer sealed, if any, with the key that
    /// sealed it, if one did (`Origin::copy`).
    pub(crate) fn unaccepted_from(&self) -> Option<(u64, Option<AccountId>)> {
        self.replay.origin.first_untrusted()
    }

    /// The key that sealed the last command, if it was sealed.
    pub(crate) fn tip_sealer(&self) -> Option<AccountId> {
        self.replay.last_sealer()
    }

    /// The cumulative command-prefix digest after the last command: what a log head commits to.
    pub(crate) fn head_link(&self) -> B256 {
        B256::from(self.replay.command_prefix)
    }

    /// The last checkpoint applied.
    pub(crate) fn last_checkpoint(&self) -> Option<&Checkpoint> {
        self.replay.last_signed_checkpoint.as_ref()
    }

    /// Records the anchor work at the last command as a checkpoint there carries it, when the copy
    /// holds no checkpoint entry there: the vault's latest checkpoint a promotion fixed may be one the
    /// outgoing key posted while no member received its entry (#595). It names the last command's
    /// sealer, the key that signed the vault's checkpoint if there is one, and the state root computed
    /// now, so it anchors only a start whose vault checkpoint is at this seq with this root, and is kept
    /// past the retention window, as a checkpoint entry is, when `keep` names its seq.
    pub(crate) fn capture_tip(&mut self) {
        let (Some(seq), Some(signer)) = (self.last_seq(), self.replay.last_sealer()) else { return };
        if self.checkpoints.contains_key(&seq) {
            return;
        }
        let root = B256::from(self.replay.engine.state_root());
        let checkpoint = crate::sequencer::replay::unheld::unheld(seq, root, signer, self.replay.domain);
        let engine = &self.replay.engine;
        if self.keeping.contains(&seq) {
            self.kept_checkpoints.entry(seq).or_insert_with(|| checkpoint.clone());
        }
        let work = Work::capture(checkpoint, self.replay.accounting, engine);
        self.checkpoints.insert(seq, (work, engine.now()));
        if self.checkpoints.len() > RETAINED_CHECKPOINTS {
            self.checkpoints.pop_first();
            self.complete = false;
        }
    }

    /// The retained, or kept, checkpoint at `seq`.
    pub(crate) fn checkpoint_at(&self, seq: u64) -> Option<&Checkpoint> {
        self.checkpoints.get(&seq).map(|(work, _)| &work.checkpoint).or_else(|| self.kept_checkpoints.get(&seq))
    }

    /// The cumulative prefix after the command at `seq`, with its stamp, while the replica retains
    /// it: one of the latest `RETAINED_LINKS` commands, or the pinned posted head.
    pub(crate) fn link_at(&self, seq: u64) -> Option<Link> {
        let index = self.links.front().and_then(|first| seq.checked_sub(first.seq));
        let retained = index.and_then(|index| self.links.get(usize::try_from(index).ok()?));
        retained.copied().or(self.pinned.filter(|pinned| pinned.seq == seq)).or_else(|| self.kept.get(&seq).copied())
    }

    /// Takes `signer`, a key the vault names that the replica's attested set predates, as attested
    /// from here on (see `Replay::admit`).
    pub(crate) fn admit(&mut self, signer: AccountId) {
        self.replay.admit(signer);
    }

    /// The latest links the replica retains, oldest first, at most `crate::liveness::link`'s window.
    pub(crate) fn retained_links(&self) -> impl Iterator<Item = &Link> {
        self.links.iter().skip(self.links.len().saturating_sub(RETAINED_LINKS / 4))
    }

    /// Keeps the posted head `(seq, link)` as one the replica holds, once its own link at `seq`
    /// is that one, so a start long after it still proves it. `None` while the replica has not
    /// reached `seq`, or no longer retains it unpinned.
    pub(crate) fn pin(&mut self, seq: u64, link: B256) -> Option<bool> {
        let held = self.link_at(seq)?;
        if held.link == link {
            self.pinned = Some(held);
        }
        Some(held.link == link)
    }

    /// The log this replica holds, ready for `Sequencer::recover_from`, with `writer` the copy's
    /// writer at its end: the replay finished as recovery's would be with the vault's finalized
    /// checkpoint `parent` as its anchor (`None` without a checkpoint service), its commands'
    /// authority pinned to `signer`, and the posted log head at `posted` located for the startup
    /// check. The copy must end at a checkpoint `signer` signed, as a handover's head does, so no
    /// command past it needs another authority; only `signer`'s outstanding checkpoints extend the
    /// anchor, since the vault takes no other key's.
    /// Error cases:
    /// - the writer and the replica end at different entries, or the copy does not end at a
    ///   checkpoint `signer` signed
    /// - the anchor is older than the checkpoints the replica retains, or an outstanding
    ///   checkpoint's signed parent breaks the chain from it
    pub(crate) fn into_log(
        mut self,
        writer: LogWriter,
        parent: Option<CheckpointParent>,
        signer: AccountId,
        posted: Option<u64>,
    ) -> anyhow::Result<ReplayedLog> {
        ensure!(
            writer.last_offset() == self.replay.last_offset,
            "the copy ends at {:?} but the replica at {:?}",
            writer.last_offset(),
            self.replay.last_offset
        );
        let domain = self.replay.domain.context("a replica replays in a deployment")?;
        let last = self.replay.last_signed_checkpoint.as_ref();
        ensure!(
            last.is_some_and(
                |cp| Some(cp.seq) == self.last_seq() && cp.signer.parse::<AccountId>().ok() == Some(signer)
            ),
            "the replica does not end at a checkpoint {signer} signed, so its tail has no authority to start from"
        );
        if let Some(parent) = parent {
            self.anchor(parent, Some(signer))?;
        }
        self.replay.origin.pin(domain, signer)?;
        self.replay.trusted_signer = Some(signer);
        if let Some(found) = posted.and_then(|seq| self.link_at(seq)) {
            (self.replay.captured_link, self.replay.captured_ts) = (Some(found.link), Some(found.ts));
        }
        Ok(ReplayedLog::live(writer, self.replay.finish()))
    }

    /// `into_log` for a promoted member (#595): the standby key the promotion made the vault's signer
    /// has signed no checkpoint, so the copy need not end at one; its commands after the last
    /// checkpoint are the outgoing key's, sealed and attested as the replay checked them, and the
    /// promotion's fixed head and checkpoint, which the caller checked the copy holds, are the start's
    /// authority. The new key joins the attested signers before it is pinned.
    /// Error cases:
    /// - the writer and the replica end at different entries
    /// - as `into_log`'s anchor and pin
    pub(crate) fn into_promoted_log(
        mut self,
        writer: LogWriter,
        parent: Option<CheckpointParent>,
        signer: AccountId,
        posted: Option<u64>,
    ) -> anyhow::Result<ReplayedLog> {
        ensure!(
            writer.last_offset() == self.replay.last_offset,
            "the copy ends at {:?} but the replica at {:?}",
            writer.last_offset(),
            self.replay.last_offset
        );
        let domain = self.replay.domain.context("a replica replays in a deployment")?;
        if let Some(parent) = parent {
            self.anchor(parent, Some(signer))?;
        }
        self.replay.admit(signer);
        self.replay.origin.pin(domain, signer)?;
        self.replay.trusted_signer = Some(signer);
        if let Some(found) = posted.and_then(|seq| self.link_at(seq)) {
            (self.replay.captured_link, self.replay.captured_ts) = (Some(found.link), Some(found.ts));
        }
        Ok(ReplayedLog::live(writer, self.replay.finish()))
    }

    /// Sets the checkpoint gate's recovery facts as a recovery replay anchored at `parent` sets
    /// them: the anchor's work and stamp, and the outstanding checkpoints after it, those `signed_by`
    /// signed when given, each extending the signed parent before it.
    /// Error cases:
    /// - the anchor is not retained, or the outstanding chain breaks
    fn anchor(&mut self, parent: CheckpointParent, signed_by: Option<AccountId>) -> anyhow::Result<()> {
        let replay = &mut self.replay;
        replay.recovery_origin = Some(parent);
        if parent.has_checkpoint {
            let (work, ts) = self
                .checkpoints
                .get(&parent.seq)
                .filter(|(work, _)| work.checkpoint.root.parse::<B256>().ok() == Some(parent.root))
                .with_context(|| {
                    format!(
                        "the vault's finalized checkpoint at seq {} is not among the checkpoints the replica retains",
                        parent.seq
                    )
                })?;
            replay.recovery_anchor = Some(work.checkpoint.clone());
            replay.recovery_anchor_ts = Some(*ts);
            replay.recovery_anchor_work = Some(work.clone());
        } else {
            ensure!(self.complete, "the replica no longer retains every checkpoint since the log's start");
        }
        let mut running = parent;
        // The outstanding work is the starting key's only: what another key signed past the vault's
        // latest the vault never accepted and never will, and the next checkpoint extends that latest.
        let outstanding = self.checkpoints.values().filter(|(work, _)| {
            work.checkpoint.parent.is_some()
                && (!parent.has_checkpoint || work.checkpoint.seq > parent.seq)
                && signed_by.is_none_or(|signer| work.checkpoint.signer.parse::<AccountId>().ok() == Some(signer))
        });
        for (work, _) in outstanding {
            ensure!(work.checkpoint.parent == Some(running), "outstanding checkpoint has a divergent signed parent");
            replay.recovery_work.push(work.clone());
            running = CheckpointParent {
                has_checkpoint: true,
                seq: work.checkpoint.seq,
                root: work.checkpoint.root.parse()?,
            };
        }
        replay.recovery_parent = Some(running);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
