//! A replay kept live: a follower's replica applies every entry its copy takes, so a start from it
//! finishes the replay the recovery of the same log would have produced, without reading the log
//! again. It retains what that finish needs: each recent checkpoint's custody work, which anchors the
//! checkpoint gate at the vault's finalized checkpoint, and the latest commands' cumulative prefixes,
//! the evidence that the replica holds a posted log head or a predecessor's tip.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context, ensure};
use engine_core::eip712::B256;
use engine_core::types::{AccountId, Millis};

use crate::checkpoints::Work;
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent, LogEntry, LogWriter};
use crate::sequencer::replay::{Replay, ReplayedLog};
use crate::stats::Stats;

/// How many checkpoints' custody work a replica retains: the vault's finalized checkpoint must be
/// among them for a start from the replica, which otherwise recovers from the log on disk.
const RETAINED_CHECKPOINTS: usize = 4096;
/// How many of the latest commands' cumulative prefixes a replica retains.
pub(crate) const RETAINED_LINKS: usize = 1 << 16;

/// A command's cumulative command-prefix digest and its sequenced time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Link {
    pub seq: u64,
    pub link: B256,
    pub ts: Millis,
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
}

impl LiveReplay {
    /// An empty replica of a log in `domain`, accepting checkpoints, typed withdrawals and command
    /// seals from the `attested` signers: the vault's recorded signers, as a follower spans every
    /// signer epoch the log does.
    pub(crate) fn new(domain: CheckpointDomain, attested: BTreeSet<AccountId>) -> Self {
        Self {
            replay: Replay::with_attested(Some(domain), attested),
            checkpoints: BTreeMap::new(),
            complete: true,
            links: VecDeque::new(),
            pinned: None,
            checked: None,
        }
    }

    /// Has the replica record each checkpoint it verifies, with the turn assignment taken there, into
    /// `checked`.
    pub(crate) fn record_checked_with(&mut self, checked: crate::log_check::answer::CheckedHeads) {
        self.checked = Some(checked);
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
        self.replay.apply(entry)?;
        match entry {
            LogEntry::Command(command) => {
                let link = Link {
                    seq: command.sequenced.seq,
                    link: B256::from(self.replay.command_prefix),
                    ts: command.sequenced.ts,
                };
                self.links.push_back(link);
                if self.links.len() > RETAINED_LINKS {
                    self.links.pop_front();
                }
            }
            LogEntry::Checkpoint(checkpoint) => {
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

    /// The state root of the entries applied so far.
    #[cfg(test)]
    pub(crate) fn state_root(&mut self) -> [u8; 32] {
        self.replay.engine.state_root()
    }

    /// The seq of the last command applied.
    pub(crate) fn last_seq(&self) -> Option<u64> {
        self.replay.engine.last_seq()
    }

    /// The cumulative command-prefix digest after the last command: what a log head commits to.
    pub(crate) fn head_link(&self) -> B256 {
        B256::from(self.replay.command_prefix)
    }

    /// The last checkpoint applied.
    pub(crate) fn last_checkpoint(&self) -> Option<&Checkpoint> {
        self.replay.last_signed_checkpoint.as_ref()
    }

    /// The retained checkpoint at `seq`.
    pub(crate) fn checkpoint_at(&self, seq: u64) -> Option<&Checkpoint> {
        self.checkpoints.get(&seq).map(|(work, _)| &work.checkpoint)
    }

    /// The cumulative prefix after the command at `seq`, with its stamp, while the replica retains
    /// it: one of the latest `RETAINED_LINKS` commands, or the pinned posted head.
    pub(crate) fn link_at(&self, seq: u64) -> Option<Link> {
        let first = self.links.front()?.seq;
        let retained = seq.checked_sub(first).and_then(|index| self.links.get(usize::try_from(index).ok()?));
        retained.copied().or(self.pinned.filter(|pinned| pinned.seq == seq))
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
    /// command past it needs another authority.
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
            self.anchor(parent)?;
        }
        self.replay.origin.pin(domain, signer)?;
        self.replay.trusted_signer = Some(signer);
        if let Some(found) = posted.and_then(|seq| self.link_at(seq)) {
            (self.replay.captured_link, self.replay.captured_ts) = (Some(found.link), Some(found.ts));
        }
        Ok(ReplayedLog::live(writer, self.replay.finish()))
    }

    /// Sets the checkpoint gate's recovery facts as a recovery replay anchored at `parent` sets
    /// them: the anchor's work and stamp, and the outstanding checkpoints after it, each extending
    /// the signed parent before it.
    /// Error cases:
    /// - the anchor is not retained, or the outstanding chain breaks
    fn anchor(&mut self, parent: CheckpointParent) -> anyhow::Result<()> {
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
        let outstanding = self.checkpoints.values().filter(|(work, _)| {
            work.checkpoint.parent.is_some() && (!parent.has_checkpoint || work.checkpoint.seq > parent.seq)
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
