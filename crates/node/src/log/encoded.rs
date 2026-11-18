//! A command entry built around its events encoded once. The sequencer encodes a command's events a
//! single time and builds the seal's link, the command digest and the log line around that encoding,
//! each byte for byte what the typed entry gives (`command_link`, `command_digest`, `LogWriter::append`),
//! so the log, its seals and its published hashes are as they were.

use anyhow::Context;
use engine_core::command::Sequenced;
use engine_core::eip712::B256;
use engine_core::event::Event;
use serde::Serialize;
use serde_json::value::RawValue;

use crate::log::CommandSeal;
use crate::log::command_seal::link_of;
use crate::log::commitment::digest_of;

/// A command entry whose events are already encoded. It serializes as `CommandEntry` does with its
/// events present.
#[derive(Serialize)]
pub(crate) struct EncodedCommand<'a> {
    #[serde(flatten)]
    pub(crate) sequenced: &'a Sequenced,
    pub(crate) events: &'a RawValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) seal: Option<&'a CommandSeal>,
}

/// `LogEntry::Command` around an encoded command.
#[derive(Serialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
enum EncodedEntry<'a> {
    Command(&'a EncodedCommand<'a>),
}

/// `events` encoded once, as a command entry carries them.
/// Error cases:
/// - an event cannot be encoded
pub(crate) fn encode_events(events: &[Event]) -> anyhow::Result<Box<RawValue>> {
    serde_json::value::to_raw_value(events).context("encoding a command's events")
}

impl<'a> EncodedCommand<'a> {
    /// The command seal chain after this command, from `previous`: `command_link`'s.
    /// Error cases:
    /// - the command cannot be encoded
    pub(crate) fn link(&self, previous: B256) -> anyhow::Result<B256> {
        link_of(previous, &(self.sequenced, self.events))
    }

    /// The command's identity in the cumulative command prefix: `command_digest`'s.
    /// Error cases:
    /// - the command cannot be encoded
    pub(crate) fn digest(&self) -> anyhow::Result<[u8; 32]> {
        digest_of(self)
    }

    /// The command's log line, with its newline: what `LogWriter::append` writes for the entry.
    /// Error cases:
    /// - the command cannot be encoded
    pub(crate) fn line(&self) -> anyhow::Result<Vec<u8>> {
        let mut line = serde_json::to_vec(&EncodedEntry::Command(self)).context("encoding log entry")?;
        line.push(b'\n');
        Ok(line)
    }

    /// `entry`'s envelope around `events`, its events encoded.
    #[cfg(test)]
    pub(crate) fn of(entry: &'a crate::log::CommandEntry, events: &'a RawValue) -> Self {
        EncodedCommand { sequenced: &entry.sequenced, events, seal: entry.seal.as_ref() }
    }
}

#[cfg(test)]
mod tests;
