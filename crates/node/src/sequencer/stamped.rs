//! A command's log entry, built around its events encoded once (`log::EncodedCommand`), and what the
//! sequencer hands back for the command: the stream message that carries its entry, so the events are
//! never copied for it.

use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use engine_core::command::Sequenced;
use engine_core::eip712::B256;
use engine_core::event::Event;
use tokio::sync::watch;

use crate::history::derived::DerivedRows;
use crate::log::{
    CheckpointDomain, CommandEntry, EncodedCommand, LogEntry, Offset, encode_events, extend_command_prefix,
};
use crate::sequencer::{Sequencer, StreamBody, StreamMsg};

/// A command the sequencer stamped: the stream message carrying its entry, which owns the events it
/// produced, or none when sequencing had stopped and it logged nothing. It reads as those events.
#[derive(Clone, Default)]
pub struct Stamped(Option<Arc<StreamMsg>>);

impl Deref for Stamped {
    type Target = [Event];

    /// The command's events; none when it logged nothing.
    fn deref(&self) -> &[Event] {
        match self.0.as_deref().map(|message| &message.body) {
            Some(StreamBody::Command(entry)) => entry.events.as_deref().unwrap_or_default(),
            _ => &[],
        }
    }
}

impl<T: AsRef<[Event]>> PartialEq<T> for Stamped {
    /// Whether the command's events are `other`'s.
    fn eq(&self, other: &T) -> bool {
        self.deref() == other.as_ref()
    }
}

impl fmt::Debug for Stamped {
    /// As the command's events.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.deref().fmt(f)
    }
}

impl Stamped {
    /// The command's events.
    pub fn as_slice(&self) -> &[Event] {
        self
    }
}

impl Sequencer {
    /// Logs `sequenced`, just applied, with `events`, those it produced: encodes the events once,
    /// seals the entry at the command seal chain after it, extends the cumulative command prefix with
    /// it and retains that, appends its line, and queues it for the stream, which takes the events.
    /// Error cases:
    /// - the command cannot be encoded, or the log cannot be written
    pub(super) fn log_command(&mut self, sequenced: Sequenced, events: Vec<Event>) -> anyhow::Result<Stamped> {
        let raw = encode_events(&events)?;
        let mut encoded = EncodedCommand { sequenced: &sequenced, events: &raw, seal: None };
        let link = encoded.link(self.command_link)?;
        let seal = self.key.seal_command(CheckpointDomain { chain_id: self.chain_id, vault: self.vault }, link);
        encoded.seal = Some(&seal);
        self.command_prefix = extend_command_prefix(self.command_prefix, encoded.digest()?);
        self.prefixes.record(sequenced.seq, B256::from(self.command_prefix));
        let offset = self.log.append_command(&encoded)?;
        self.command_link = link;
        let entry = CommandEntry { sequenced, events: Some(events), seal: Some(seal) };
        let message = Arc::new(StreamMsg::new(offset, LogEntry::Command(entry)));
        self.outbox.push(message.clone());
        Ok(Stamped(Some(message)))
    }

    /// Appends an entry to the log and queues it for the stream at its offset.
    /// Error cases:
    /// - the log cannot be written
    pub(super) fn record(&mut self, entry: LogEntry) -> anyhow::Result<Offset> {
        let offset = self.log.append(&entry)?;
        self.outbox.push(Arc::new(StreamMsg::new(offset, entry)));
        Ok(offset)
    }

    /// A receiver of the newest commands' derived rows, which the sequencer publishes as it applies
    /// each command.
    pub(crate) fn derived_rows(&self) -> watch::Receiver<DerivedRows> {
        self.derived.subscribe()
    }
}
