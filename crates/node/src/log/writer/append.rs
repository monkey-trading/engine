//! Appending entries: each entry's line, and the open segment's bookkeeping for it. A command whose
//! events are already encoded (`EncodedCommand`) goes through the same path as a typed entry.

use anyhow::{Context, ensure};

use crate::log::catalog::CheckpointLocator;
use crate::log::{Checkpoint, EncodedCommand, LogEntry, LogWriter, Offset};

/// What an appended line holds, as the segment's bookkeeping reads it.
#[derive(Clone, Copy)]
enum Appended<'a> {
    Command(u64),
    Checkpoint(&'a Checkpoint),
    Withdrawal,
}

impl LogWriter {
    /// Buffers one entry as a JSON line, and returns its offset. A command first completes the
    /// current segment if it is full.
    /// Error cases:
    /// - completing the current segment or starting a new one fails
    /// - a withdrawal or checkpoint comes before any command in this segment
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
        if let Appended::Command(seq) = appended
            && self.open.as_ref().is_none_or(|(segment, _)| segment.commands >= self.segment_commands)
        {
            self.start_segment(seq)?;
        }
        let (segment, out) = self.open.as_mut().context("log entry before the first command of its segment")?;
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
