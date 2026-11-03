//! Whether the operator's stream extends the standby's copy. The enclave seals every command over
//! `command_link`, a hash chain that restarts at each checkpoint, so the link after the copy's last
//! entry pins the copy's history. A resumed stream starts at the copy's last command and must
//! repeat it, and the entries after it, exactly; every later command must carry the followed
//! enclave's seal over the link that extends the copy. A signed heartbeat naming a head behind the
//! copy, a repeat that differs, or an entry that does not chain means the operator's log no longer
//! holds the copy's history. The copy then takes nothing more, and stays as it is.

use std::fmt;
use std::path::Path;

use engine_core::eip712::B256;
use engine_core::types::AccountId;

use crate::log::command_seal::{checkpoint_link, command_link};
use crate::log::{CheckpointDomain, LogEntry, Offset, offset_of};

/// The copy's command-origin chain, and its last command with the entries after it.
#[derive(Clone)]
pub(super) struct Chain {
    domain: CheckpointDomain,
    enclave: AccountId,
    /// The link after the copy's last entry.
    link: B256,
    /// The copy's last command, then every entry after it, which a resumed stream repeats first.
    tail: Vec<(Offset, LogEntry)>,
    /// How many tail entries this connection has repeated; `None` once it has repeated them all.
    repeat: Option<usize>,
}

/// What an entry the operator streams is to the copy.
#[derive(Debug, PartialEq)]
pub(super) enum Step {
    /// One of the copy's own entries, repeated: there is nothing to append.
    Repeated,
    /// The copy's next entry, and the link after it.
    Extends(B256),
}

/// The operator's log no longer extends the copy: the copy's head, and what showed it.
#[derive(Debug, PartialEq)]
pub(super) struct Rollback {
    pub copy_head: Option<Offset>,
    pub evidence: String,
}

impl fmt::Display for Rollback {
    /// The copy's head and the evidence.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.copy_head {
            Some(head) => write!(formatter, "the operator's log does not extend the copy, which ends at {head}: "),
            None => write!(formatter, "the operator's log does not extend the empty copy: "),
        }?;
        formatter.write_str(&self.evidence)
    }
}

impl std::error::Error for Rollback {}

impl Chain {
    /// An empty copy's chain, for the commands `enclave` seals in `domain`.
    pub(super) fn new(domain: CheckpointDomain, enclave: AccountId) -> Self {
        Self { domain, enclave, link: B256::default(), tail: Vec::new(), repeat: None }
    }

    /// Takes the copy's next entry as read from its own files, which hold only what the stream or
    /// the archive gave it: its links are computed, not checked.
    /// Error cases:
    /// - the entry does not follow the last one, or its link cannot be computed
    pub(super) fn record(&mut self, entry: LogEntry) -> anyhow::Result<()> {
        let offset = offset_of(self.head(), &entry)?;
        let link = match &entry {
            LogEntry::Command(command) => command_link(self.link, command)?,
            LogEntry::Checkpoint(checkpoint) => checkpoint_link(checkpoint)?,
            LogEntry::Withdrawal(_) => self.link,
        };
        self.extend(offset, entry, link);
        Ok(())
    }

    /// Takes an entry `check` found to extend the copy, once the copy has it.
    pub(super) fn extend(&mut self, offset: Offset, entry: LogEntry, link: B256) {
        if matches!(entry, LogEntry::Command(_)) {
            self.tail.clear();
        }
        self.tail.push((offset, entry));
        self.link = link;
    }

    /// The offset of the copy's last entry, or `None` for an empty copy.
    pub(super) fn head(&self) -> Option<Offset> {
        self.tail.last().map(|(offset, _)| *offset)
    }

    /// The seq a resumed stream starts at: the copy's last command's, or 0 for an empty copy.
    pub(super) fn resume_seq(&self) -> u64 {
        self.tail.first().map_or(0, |(offset, _)| offset.seq)
    }

    /// Starts a connection, which repeats the tail before anything new.
    pub(super) fn begin(&mut self) {
        self.repeat = Some(0);
    }

    /// Whether this connection has repeated the whole tail, or for an empty copy has extended it:
    /// the operator's log holds the copy's.
    pub(super) fn confirmed(&self) -> bool {
        self.repeat.is_none()
    }

    /// Checks a signed heartbeat's head: an operator's flushed head never trails what it streamed.
    /// Error cases:
    /// - the head is behind the copy's
    pub(super) fn heartbeat(&self, head: Option<Offset>) -> Result<(), Rollback> {
        match (self.head(), head) {
            (Some(copy), Some(head)) if head < copy => Err(self.rollback(format!(
                "its signed head is {head}, so it no longer holds the copy's entries after {head} through {copy}"
            ))),
            (Some(copy), None) => Err(self.rollback(format!(
                "its signed head is empty, so it no longer holds the copy's entries through {copy}"
            ))),
            _ => Ok(()),
        }
    }

    /// Checks the operator's next streamed entry: a repeat of the tail, or the copy's next entry.
    /// Error cases:
    /// - a repeat differs from the copy's entry, or skips one
    /// - a new entry does not follow the copy's head, or a command does not carry the followed
    ///   enclave's seal, for this deployment, over the link that extends the copy
    pub(super) fn check(&mut self, offset: Offset, entry: &LogEntry) -> Result<Step, Rollback> {
        if let Some(index) = self.repeat {
            let Some((expected, copied)) = self.tail.get(index) else {
                // An empty copy has nothing to repeat: its first entry is new.
                self.repeat = None;
                return self.check(offset, entry);
            };
            if offset != *expected || entry != copied {
                let copy = self.head().expect("a repeat has a tail");
                let seen = if offset == *expected {
                    format!("its entry at {expected} differs from the copy's")
                } else {
                    format!("it sent {offset} where the copy holds {expected}")
                };
                return Err(self.rollback(format!(
                    "{seen}, so it no longer holds the copy's entries from {expected} through {copy}"
                )));
            }
            self.repeat = (index + 1 < self.tail.len()).then_some(index + 1);
            return Ok(Step::Repeated);
        }
        let head = self.head();
        if offset_of(head, entry).ok() != Some(offset) {
            return Err(self.rollback(format!("its entry at {offset} does not follow the copy's head")));
        }
        let not_next = |why: String| self.rollback(format!("its entry at {offset} does not extend the copy: {why}"));
        match entry {
            LogEntry::Command(command) => {
                let next = head.map_or(0, |head| head.seq + 1);
                if command.sequenced.seq != next {
                    return Err(not_next(format!("the copy's next command is seq {next}")));
                }
                let seal = command.seal.as_ref().ok_or_else(|| not_next("it carries no command seal".into()))?;
                if seal.domain != self.domain || seal.signer != self.enclave {
                    return Err(not_next(format!(
                        "it is sealed by {} for chain {} and vault {}, not by the followed enclave {} for this deployment",
                        seal.signer, seal.domain.chain_id, seal.domain.vault, self.enclave
                    )));
                }
                let link = command_link(self.link, command).map_err(|e| not_next(format!("{e:#}")))?;
                seal.verify(link).map_err(|_| not_next("its seal does not chain onto the copy's history".into()))?;
                Ok(Step::Extends(link))
            }
            LogEntry::Checkpoint(checkpoint) => {
                if checkpoint.seq != offset.seq {
                    return Err(not_next(format!("it is a checkpoint for seq {}", checkpoint.seq)));
                }
                checkpoint_link(checkpoint).map(Step::Extends).map_err(|e| not_next(format!("{e:#}")))
            }
            LogEntry::Withdrawal(_) => Ok(Step::Extends(self.link)),
        }
    }

    /// Takes the entries of the segment file at `path` as the copy's next ones, only if every one
    /// of them extends it; otherwise the chain is unchanged.
    /// Error cases:
    /// - the file cannot be read, or an entry does not extend the copy (a `Rollback` in the chain)
    pub(super) fn extend_with(&mut self, path: &Path) -> anyhow::Result<()> {
        let mut next = self.clone();
        next.repeat = None;
        crate::log::read_file(path, |entry| {
            let offset = offset_of(next.head(), &entry)?;
            let Step::Extends(link) = next.check(offset, &entry)? else {
                unreachable!("a chain with no repeat pending takes only new entries")
            };
            next.extend(offset, entry, link);
            Ok(())
        })?;
        *self = next;
        Ok(())
    }

    /// A rollback seen against the copy's current head.
    fn rollback(&self, evidence: String) -> Rollback {
        Rollback { copy_head: self.head(), evidence }
    }
}

/// The rollback anywhere in `error`'s chain of causes, if any.
pub(super) fn rollback_of(error: &anyhow::Error) -> Option<&Rollback> {
    error.chain().find_map(|cause| cause.downcast_ref::<Rollback>())
}

#[cfg(test)]
mod tests;
