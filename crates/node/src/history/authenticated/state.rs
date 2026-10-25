//! Bounded progress identities and the required command-prefix commitment.

use anyhow::{Context, Result, ensure};
use engine_core::types::Millis;
use serde::{Deserialize, Deserializer, Serialize};

use crate::authenticated::{Root, Store};
use crate::log::{CommandEntry, Offset, command_digest};

pub(super) const STATE_KEY: &[u8] = b"history/v2/state";
const FORMAT: u32 = 2;

/// A bounded identity of the complete canonical command and logged events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandId {
    pub seq: u64,
    pub ts: Millis,
    pub digest: [u8; 32],
}

/// Certificate payload supplied only by parent-authenticated execution or signatures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Head {
    pub root: Root,
    #[serde(deserialize_with = "required_command")]
    pub last_command: Option<CommandId>,
    pub command_prefix: [u8; 32],
    pub queue_lo: u64,
    pub queue_hi: u64,
    pub next_generation: u64,
    pub retention_ms: Millis,
}

/// Pending roots may contain partial expiry/payload/list work and cannot become public heads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    format: u32,
    pub complete: bool,
    #[serde(deserialize_with = "required_command")]
    pub last_command: Option<CommandId>,
    pub command_prefix: [u8; 32],
    pub queue_lo: u64,
    pub queue_hi: u64,
    pub next_generation: u64,
    pub retention_ms: Millis,
}

impl CommandId {
    /// Uses the parent's shared full typed-input digest, never a truncated event vector.
    /// Error cases:
    /// - canonical command encoding fails
    pub fn of(command: &CommandEntry) -> Result<Self> {
        Ok(Self { seq: command.sequenced.seq, ts: command.sequenced.ts, digest: command_digest(command)? })
    }
}

impl Head {
    /// Resumes after the indexed command without dropping same-sequence checkpoint/release fences.
    pub fn resume_at(&self) -> Offset {
        self.last_seq().map_or(Offset::start_of(0), |seq| Offset { seq, index: 1 })
    }

    /// Constructs the sole canonical virtual head whose fixed state leaf is absent.
    /// Error cases:
    /// - retention is not positive
    pub fn empty(namespace: [u8; 32], retention_ms: Millis) -> Result<Self> {
        ensure!(retention_ms > 0, "history retention must be positive");
        Ok(Self {
            root: Root::empty(namespace),
            last_command: None,
            command_prefix: [0; 32],
            queue_lo: 0,
            queue_hi: 0,
            next_generation: 0,
            retention_ms,
        })
    }

    /// Returns the last complete indexed command sequence.
    pub fn last_seq(&self) -> Option<u64> {
        self.last_command.map(|command| command.seq)
    }

    /// Returns the corresponding command time, including for commands with no logged events.
    pub fn last_ts(&self) -> Option<Millis> {
        self.last_command.map(|command| command.ts)
    }

    /// Builds only the complete root-independent state expected at a public head.
    pub(super) fn state(&self) -> State {
        State {
            format: FORMAT,
            complete: true,
            last_command: self.last_command,
            command_prefix: self.command_prefix,
            queue_lo: self.queue_lo,
            queue_hi: self.queue_hi,
            next_generation: self.next_generation,
            retention_ms: self.retention_ms,
        }
    }
}

impl State {
    /// Wraps complete progress with its final durable root; pending states have no Head conversion.
    /// Error cases:
    /// - invalid or incomplete progress
    pub fn head(self, root: Root) -> Result<Head> {
        self.validate()?;
        ensure!(self.complete, "pending history state cannot be published");
        Ok(Head {
            root,
            last_command: self.last_command,
            command_prefix: self.command_prefix,
            queue_lo: self.queue_lo,
            queue_hi: self.queue_hi,
            next_generation: self.next_generation,
            retention_ms: self.retention_ms,
        })
    }

    /// Checks fixed progress without enumerating any list, queue, file, or SQL rows.
    /// Error cases:
    /// - incompatible format, invalid retention/time, reversed queue, or noncanonical empty progress
    pub fn validate(&self) -> Result<()> {
        ensure!(self.format == FORMAT, "unknown authenticated history format");
        ensure!(self.retention_ms > 0, "history retention must be positive");
        ensure!(self.queue_lo <= self.queue_hi, "history queue interval is reversed");
        match self.last_command {
            Some(last) => ensure!(last.ts >= 0, "negative history command time"),
            None => ensure!(
                self.complete
                    && self.command_prefix == [0; 32]
                    && self.queue_lo == 0
                    && self.queue_hi == 0
                    && self.next_generation == 0,
                "noncanonical empty history progress"
            ),
        }
        Ok(())
    }

    /// Computes the exact inclusive retention boundary.
    pub fn horizon(&self) -> Millis {
        self.last_command.map_or(i64::MIN, |last| last.ts.saturating_sub(self.retention_ms))
    }
}

/// Proves the exact fixed metadata of a COMPLETE caller-authenticated head.
/// Error cases:
/// - missing/unproved/malformed state, pending state, or any certified-field mismatch
pub(super) fn checked_state(store: &Store, head: &Head) -> Result<State> {
    let expected = head.state();
    expected.validate()?;
    match store.read(&head.root, STATE_KEY).context("proving history state")? {
        Some(bytes) => {
            let actual: State = serde_json::from_slice(&bytes).context("decoding history state")?;
            actual.validate()?;
            ensure!(actual.complete, "pending history state cannot be resumed");
            ensure!(actual == expected, "history state differs from certified head");
        }
        None => ensure!(*head == Head::empty(head.root.namespace, head.retention_ms)?, "missing history state"),
    }
    Ok(expected)
}

/// Keeps nullable progress explicitly required on disk and in certificates.
/// Error cases:
/// - missing field or malformed command identity
fn required_command<'de, D: Deserializer<'de>>(decoder: D) -> std::result::Result<Option<CommandId>, D::Error> {
    Option::<CommandId>::deserialize(decoder)
}
