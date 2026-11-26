//! A replay of the log on this host from a daily snapshot's checkpoint on: the snapshot's engine and the replay
//! state it carries stand for every entry before the checkpoint, which is not read (`Replay::resume`), and the
//! entries after it are read and replayed as a whole replay replays them.

use anyhow::{Context, ensure};

use crate::daily_snapshots::{DailySnapshot, Signed};
use crate::log::{LogEntry, SealChecks, SegmentedLog};
use crate::sequencer::replay::Replay;

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

/// The first seq from which the log in `dir` holds, checked, the entries before a daily snapshot's checkpoint that a
/// replay going on from it did not read: those of the commands whose links its carried state names, `links`, ending
/// at the checkpoint's command, from the one after the first link, or after the last segment `held` lists that this
/// host no longer holds.
/// Each segment is read and its bytes checked against `held`'s descriptor of it, the one the node then serves it by,
/// so what is checked is what is served; each command's link must extend the one before it as `links` says, so its
/// bytes are those the snapshot's signer sequenced, and each checkpoint's and withdrawal's signature must be by one
/// of `signers` in `domain`. None when the links name too few commands, `held` lists none of them, or a read or
/// check fails: the start then vouches only for what it replayed.
pub(crate) fn verified_back(
    dir: &std::path::Path,
    links: &[crate::sequencer::replay::live::Link],
    held: &crate::log::Manifest,
    (domain, signers): (crate::log::CheckpointDomain, &std::collections::BTreeSet<engine_core::types::AccountId>),
) -> Option<u64> {
    let (first, last) = (links.first()?.seq.checked_add(1)?, links.last()?.seq);
    let linked: std::collections::BTreeMap<u64, [u8; 32]> = links.iter().map(|link| (link.seq, link.link.0)).collect();
    let listed = held.segments.iter().chain(held.active.as_ref().map(|active| &active.info));
    let window: Vec<_> = listed.filter(|info| info.last_seq >= first && info.first_seq <= last).collect();
    let checked = (|| {
        let (mut verified, mut at) = (None, None);
        for info in window {
            // The checked range starts again after a segment this host no longer holds.
            let bytes = match std::fs::read(dir.join(&info.file)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    (verified, at) = (None, None);
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
                    return Ok(verified);
                }
                checked_entry(&entry, seq, &linked, (domain, signers))?;
                if matches!(entry, LogEntry::Command(_)) {
                    verified.get_or_insert(seq);
                }
            }
        }
        anyhow::Ok(verified)
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

/// Checks `entry`, at `seq`, of the log before a daily snapshot's checkpoint: a command's link must extend the one
/// before it as `linked` says, and a checkpoint's or withdrawal's signature must be by one of `signers` in `domain`.
/// Error cases:
/// - any of those fails, or the command's link before it is not in `linked`
fn checked_entry(
    entry: &LogEntry,
    seq: u64,
    linked: &std::collections::BTreeMap<u64, [u8; 32]>,
    (domain, signers): (crate::log::CheckpointDomain, &std::collections::BTreeSet<engine_core::types::AccountId>),
) -> anyhow::Result<()> {
    match entry {
        LogEntry::Command(command) => {
            let previous = linked.get(&(seq - 1)).context("no carried link before the command")?;
            let digest = crate::log::command_digest(command)?;
            ensure!(
                Some(&crate::log::extend_command_prefix(*previous, digest)) == linked.get(&seq),
                "the command at seq {seq} is not the one the snapshot's links name"
            );
        }
        LogEntry::Checkpoint(checkpoint) => {
            crate::checkpoints::signature::verify_domain(checkpoint, domain)?;
            let signer = checkpoint.signer.parse().ok();
            ensure!(
                signer.is_some_and(|signer| signers.contains(&signer)),
                "the checkpoint at seq {seq} is by another signer"
            );
        }
        LogEntry::Withdrawal(withdrawal) => {
            let verifies = |signer: &engine_core::types::AccountId| {
                crate::checkpoints::signature::verify_withdrawal(withdrawal, Some(domain), Some(*signer)).is_ok()
            };
            ensure!(signers.iter().any(verifies), "the withdrawal entry at seq {seq} is by another signer");
        }
    }
    Ok(())
}
