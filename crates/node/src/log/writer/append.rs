//! Appending entries: each entry's line, and the open segment's bookkeeping for it. A command whose
//! events are already encoded (`EncodedCommand`) goes through the same path as a typed entry.

use anyhow::{Context, ensure};

use crate::log::catalog::CheckpointLocator;
use crate::log::segments::MAX_SEGMENT_BYTES;
use crate::log::{Checkpoint, EncodedCommand, LogEntry, LogWriter, Offset};

/// What an appended line holds, as the segment's bookkeeping reads it.
#[derive(Clone, Copy)]
enum Appended<'a> {
    Command(u64),
    Checkpoint(&'a Checkpoint),
    Withdrawal,
}

impl LogWriter {
    /// Whether a checkpoint or withdrawal entry has a place yet: the open segment holds a command for it
    /// to follow. A log reopened after a crash during rotation may end with a listed segment, which is
    /// never written again, and nothing, or an empty segment, after it; the entry then waits for the
    /// next command, which starts the segment.
    pub(crate) fn takes_non_command(&self) -> bool {
        self.open.as_ref().is_some_and(|(segment, _)| segment.commands > 0)
    }

    /// Buffers one entry as a JSON line, and returns its offset. A command first completes the
    /// current segment if it is full, by its commands or by its line's bytes (`MAX_SEGMENT_BYTES`).
    /// Error cases:
    /// - completing the current segment or starting a new one fails
    /// - a withdrawal or checkpoint comes before any command in this segment, including a
    ///   segment recovery found created and still empty
    /// - the entry cannot be encoded
    pub fn append(&mut self, entry: &LogEntry) -> anyhow::Result<Offset> {
        let mut line = serde_json::to_vec(entry).context("encoding log entry")?;
        line.push(b'\n');
        let appended = match entry {
            LogEntry::Command(command) => Appended::Command(command.sequenced.seq),
            LogEntry::Checkpoint(checkpoint) => Appended::Checkpoint(checkpoint),
            LogEntry::Withdrawal(_) => Appended::Withdrawal,
        };
        self.append_line(appended, &line)
    }

    /// `append` for a command whose events are already encoded: the same line, built around them.
    /// Error cases:
    /// - as `append`
    pub(crate) fn append_command(&mut self, command: &EncodedCommand) -> anyhow::Result<Offset> {
        self.append_line(Appended::Command(command.sequenced.seq), &command.line()?)
    }

    /// Buffers `line`, the entry `appended` describes, and returns its offset.
    /// Error cases:
    /// - as `append`
    fn append_line(&mut self, appended: Appended<'_>, line: &[u8]) -> anyhow::Result<Offset> {
        ensure!(!self.listing_unsettled, "the open segment's listing may be durable, so nothing is appended to it");
        let offset = match appended {
            Appended::Command(seq) => Offset::start_of(seq),
            _ => self.last.context("log entry before the first command of its segment")?.next(),
        };
        let full = |commands: u64, bytes: u64| {
            commands >= self.segment_commands || (commands > 0 && bytes + line.len() as u64 > MAX_SEGMENT_BYTES)
        };
        if let Appended::Command(seq) = appended
            && self.open.as_ref().is_none_or(|(segment, _)| full(segment.commands, segment.bytes))
        {
            self.start_segment(seq)?;
        }
        let (segment, out) = self.open.as_mut().context("log entry before the first command of its segment")?;
        // Every segment starts with a command, which the open check requires. A segment a crash left
        // linked and empty is open here with no command, and its predecessor is already listed.
        ensure!(
            matches!(appended, Appended::Command(_)) || segment.commands > 0,
            "log entry before the first command of its segment"
        );
        self.unsynced = true;
        self.appended += 1;
        self.appended_bytes += line.len() as u64;
        out.push(line);
        if let Appended::Checkpoint(checkpoint) = appended {
            segment.checkpoints.push(CheckpointLocator {
                segment_first_seq: segment.first_seq,
                offset,
                start: segment.bytes,
                end: segment.bytes + line.len() as u64,
                checkpoint: checkpoint.clone(),
            });
        }
        segment.record(
            line,
            match appended {
                Appended::Command(seq) => Some(seq),
                _ => None,
            },
        );
        self.last = Some(offset);
        Ok(offset)
    }
}
