//! A replay of the log on this host from a daily snapshot's checkpoint on: the snapshot's engine and the replay
//! state it carries stand for every entry before the checkpoint, which is not read (`Replay::resume`), and the
//! entries after it are read and replayed as a whole replay replays them.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::daily_snapshots::{DailySnapshot, Signed};
use crate::log::{LogEntry, SealChecks, SegmentedLog};
use crate::sequencer::replay::Replay;
use crate::sequencer::replay::live::Link;

/// Marks a replay that cannot go on from a daily snapshot at all, before it reads the log after it: the
/// snapshot carries no replay state, or does not stand for its engine and checkpoint (`Replay::resume`). A
/// caller that holds the whole log can replay it instead.
#[derive(Debug)]
pub(crate) struct SnapshotRefused;

impl std::fmt::Display for SnapshotRefused {
    /// Says the replay cannot go on from the snapshot.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the replay cannot go on from the daily snapshot")
    }
}

impl Replay {
    /// This replay, set up with its trust, going on from `signed`'s checkpoint, then reading the log in `dir`
    /// from the entry after it to its end, each entry handed to `observe` before the replay applies it, its
    /// seals and roots checked as `seals` says (`check_ahead`) and its read settled (`settled`). The segments
    /// before the one holding the checkpoint are not read, and need not be on this host.
    /// Error cases:
    /// - the snapshot carries no replay state, or it does not stand for the snapshot's engine and checkpoint, or
    ///   the engine's root is not the checkpoint's (`resume`), each under `SnapshotRefused`
    /// - the log holds another entry at the snapshot's offset than its checkpoint, or ends before it
    /// - the log cannot be read from there (`SegmentedLog::open_ahead_from`), an entry fails replay, or `observe`
    ///   fails
    pub(in crate::sequencer) fn replay_after(
        self,
        signed: Signed,
        dir: &std::path::Path,
        seals: SealChecks,
        mut observe: impl FnMut(&LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<(Self, SegmentedLog)> {
        let DailySnapshot { offset, checkpoint, engine, carried, .. } = signed.snapshot;
        let resumed = carried
            .context("the daily snapshot carries no replay state")
            .and_then(|carried| self.resume(engine, carried, checkpoint.clone(), offset));
        let mut replay = resumed.context(SnapshotRefused)?;
        replay.check_ahead(dir, seals);
        let mut at_checkpoint = None;
        let read = SegmentedLog::open_ahead_from(dir, offset, |entry| {
            if at_checkpoint.is_none() {
                let held = matches!(&entry, LogEntry::Checkpoint(held) if *held == checkpoint);
                at_checkpoint = Some(held);
                ensure!(held, "the log holds another entry at {offset} than the daily snapshot's checkpoint");
                return Ok(());
            }
            observe(&entry)?;
            replay.apply(&entry)
        })
        .and_then(|log| {
            ensure!(at_checkpoint.is_some(), "the log ends before the daily snapshot's checkpoint at {offset}");
            Ok(log)
        });
        let log = replay.settled(read)?;
        Ok((replay, log))
    }
}

/// What a replay that went on from a daily snapshot holds of the log before the snapshot's checkpoint, which it did
/// not read: the links its carried state names, each protocol version's activation, and every checkpoint's root.
pub(crate) struct Behind<'a> {
    pub(crate) links: &'a [Link],
    pub(crate) activations: &'a [(ProtocolVersion, u64)],
    pub(crate) roots: &'a BTreeMap<u64, B256>,
}

/// The first seq from which the log in `dir` holds, checked, the entries before a daily snapshot's checkpoint that a
/// replay going on from it did not read: those of the commands whose links `behind` names, ending at the checkpoint's
/// command, from the one after the first link and after the version that keeps every approval in the engine
/// (`ProtocolVersion::V22`, so every withdrawal entry is a typed or a deferred one), or after the last segment `held`
/// lists that this host no longer holds.
/// Each segment is read and its bytes checked against `held`'s descriptor of it, the one the node then serves it by,
/// so what is checked is what is served; each command's link must extend the one before it as `behind` says, so its
/// bytes, events and seal are those the snapshot's signer sequenced; each checkpoint must follow its own command,
/// with the root `behind` holds for it, and none `behind` holds may be missing; and each withdrawal must follow its
/// approval and the checkpoint its authorization names, with that checkpoint's root, where they are among the commands
/// checked. Each checkpoint's and
/// withdrawal's signature must be by one of `signers` in `domain`. None when the links name too few commands, `held`
/// lists none of them, or a read or check fails: the start then vouches only for what it replayed.
pub(crate) fn verified_back(
    dir: &std::path::Path,
    behind: Behind<'_>,
    held: &crate::log::Manifest,
    (domain, signers): (crate::log::CheckpointDomain, &BTreeSet<AccountId>),
) -> Option<u64> {
    let typed = behind.activations.iter().find(|(version, _)| *version >= ProtocolVersion::V22)?.1;
    let (first, last) = (behind.links.first()?.seq.max(typed).checked_add(1)?, behind.links.last()?.seq);
    let listed = held.segments.iter().chain(held.active.as_ref().map(|active| &active.info));
    let segments: Vec<_> = listed.filter(|info| info.last_seq >= first && info.first_seq <= last).collect();
    let checked = (|| {
        let mut window = Window::new(&behind, (domain, signers));
        let mut at = None;
        for info in segments {
            // The checked range starts again after a segment this host no longer holds.
            let bytes = match std::fs::read(dir.join(&info.file)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    (window, at) = (Window::new(&behind, (domain, signers)), None);
                    continue;
                }
                read => read.with_context(|| format!("reading {}", info.file))?,
            };
            let bytes = bytes.get(..usize::try_from(info.bytes)?).context("a segment shorter than its listing")?;
            let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes));
            ensure!(digest == info.sha256, "{} differs from the listing the node serves it by", info.file);
            for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
                let entry: LogEntry = serde_json::from_slice(line).context("a line is not a log entry")?;
                if let LogEntry::Command(command) = &entry {
                    at = Some(command.sequenced.seq);
                }
                let Some(seq) = at.filter(|seq| *seq >= first) else { continue };
                if seq > last {
                    return window.ended(last);
                }
                window.check(&entry, seq)?;
            }
        }
        window.ended(last)
    })();
    checked
        .inspect_err(|e| {
            tracing::warn!(
                "the log before the daily snapshot's checkpoint does not check, so none of it is served: {e:#}"
            )
        })
        .ok()
        .flatten()
}

/// A check of the log before a daily snapshot's checkpoint (`verified_back`) that owns what it reads, to run on a
/// thread of its own after the start.
pub(crate) struct Back {
    dir: std::path::PathBuf,
    links: Vec<Link>,
    activations: Vec<(ProtocolVersion, u64)>,
    /// The roots of the checkpoints from the first link's command on, the only ones the check reads.
    roots: BTreeMap<u64, B256>,
    held: crate::log::Manifest,
    trust: (crate::log::CheckpointDomain, BTreeSet<AccountId>),
}

impl Back {
    /// A check of the log in `dir` before a daily snapshot's checkpoint, against `links`, `activations` and `roots`,
    /// what the replay from it holds of the log before the checkpoint, the descriptors `held` and the signers `trust`
    /// names in its domain.
    pub(crate) fn new(
        dir: &std::path::Path,
        links: Vec<Link>,
        activations: &[(ProtocolVersion, u64)],
        roots: &BTreeMap<u64, B256>,
        held: crate::log::Manifest,
        trust: (crate::log::CheckpointDomain, BTreeSet<AccountId>),
    ) -> Self {
        let from = links.first().map_or(u64::MAX, |link| link.seq);
        let roots = roots.range(from..).map(|(seq, root)| (*seq, *root)).collect();
        Self { dir: dir.to_owned(), links, activations: activations.to_vec(), roots, held, trust }
    }

    /// The first seq the check verified, as `verified_back` gives it.
    pub(crate) fn check(self) -> Option<u64> {
        let behind = Behind { links: &self.links, activations: &self.activations, roots: &self.roots };
        verified_back(&self.dir, behind, &self.held, (self.trust.0, &self.trust.1))
    }
}

/// The entries of the log before a daily snapshot's checkpoint checked so far, from the first command checked.
struct Window<'a> {
    linked: BTreeMap<u64, [u8; 32]>,
    roots: &'a BTreeMap<u64, B256>,
    trust: (crate::log::CheckpointDomain, &'a BTreeSet<AccountId>),
    /// The first command checked.
    from: Option<u64>,
    /// The seq of each checkpoint checked.
    checkpoints: BTreeSet<u64>,
    /// Each withdrawal a checked command approved, by account and nonce: its amount and the command's seq.
    approved: BTreeMap<(AccountId, u64), (Fixed, u64)>,
}

impl<'a> Window<'a> {
    /// A window that has checked nothing, against `behind` and the signers `trust` names in its domain.
    fn new(behind: &Behind<'a>, trust: (crate::log::CheckpointDomain, &'a BTreeSet<AccountId>)) -> Self {
        let linked = behind.links.iter().map(|link| (link.seq, link.link.0)).collect();
        let (from, checkpoints, approved) = (None, BTreeSet::new(), BTreeMap::new());
        Self { linked, roots: behind.roots, trust, from, checkpoints, approved }
    }

    /// Checks `entry`, after the command at `seq`, as `verified_back` says.
    /// Error cases:
    /// - any check `verified_back` names fails for it, or the command's link before it is not in `linked`
    fn check(&mut self, entry: &LogEntry, seq: u64) -> anyhow::Result<()> {
        let (domain, signers) = self.trust;
        match entry {
            LogEntry::Command(command) => {
                let previous = self.linked.get(&(seq - 1)).context("no carried link before the command")?;
                let digest = crate::log::command_digest(command)?;
                ensure!(
                    Some(&crate::log::extend_command_prefix(*previous, digest)) == self.linked.get(&seq),
                    "the command at seq {seq} is not the one the snapshot's links name"
                );
                for event in command.events.iter().flatten() {
                    if let Event::WithdrawalApproved { account, amount, nonce } = event {
                        self.approved.insert((*account, *nonce), (*amount, seq));
                    }
                }
                self.from.get_or_insert(seq);
            }
            LogEntry::Checkpoint(checkpoint) => {
                crate::checkpoints::signature::verify_domain(checkpoint, domain)?;
                let signer = checkpoint.signer.parse().ok();
                ensure!(
                    signer.is_some_and(|signer| signers.contains(&signer)),
                    "the checkpoint at seq {seq} is by another signer"
                );
                ensure!(checkpoint.seq == seq, "a checkpoint for seq {} after seq {seq}", checkpoint.seq);
                let root = checkpoint.root.parse::<B256>().ok();
                ensure!(
                    root.is_some() && root.as_ref() == self.roots.get(&seq),
                    "the checkpoint at seq {seq} has another root than the snapshot's replay state holds"
                );
                self.checkpoints.insert(seq);
            }
            LogEntry::Withdrawal(withdrawal) => {
                let verifies = |signer: &AccountId| {
                    crate::checkpoints::signature::verify_withdrawal(withdrawal, Some(domain), Some(*signer)).is_ok()
                };
                ensure!(signers.iter().any(verifies), "the withdrawal entry at seq {seq} is by another signer");
                let approved = withdrawal.approved_seq.context("an untyped withdrawal entry after V22")?;
                ensure!(approved <= seq, "the withdrawal entry at seq {seq} comes before its approval at {approved}");
                let Some(auth) = &withdrawal.authorization else { return Ok(()) };
                let key = (withdrawal.account, withdrawal.nonce);
                ensure!(
                    self.from.is_none_or(|from| approved < from)
                        || self.approved.get(&key) == Some(&(withdrawal.amount, approved)),
                    "the withdrawal entry at seq {seq} is not the one its command approved"
                );
                let named = auth.checkpoint_seq;
                ensure!(
                    self.from.is_none_or(|from| named < from)
                        || (self.checkpoints.contains(&named) && self.roots.get(&named) == Some(&auth.checkpoint_root)),
                    "the withdrawal entry at seq {seq} names a checkpoint at {named} the log does not hold before it"
                );
            }
        }
        Ok(())
    }

    /// The first command checked, once the window reached the snapshot's command at `last`.
    /// Error cases:
    /// - a checkpoint the snapshot's replay state holds between the first command checked and `last` is missing
    fn ended(&self, last: u64) -> anyhow::Result<Option<u64>> {
        if let Some(from) = self.from {
            let missing = self.roots.range(from..last).find(|(seq, _)| !self.checkpoints.contains(seq));
            ensure!(missing.is_none(), "the checkpoint at seq {:?} is missing", missing.map(|(seq, _)| seq));
        }
        Ok(self.from)
    }
}
