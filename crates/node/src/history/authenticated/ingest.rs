//! Complete commands publish once after bounded, proof-verified private expiry and append stages.

use anyhow::{Context, Result, ensure};

use crate::authenticated::Root;
use crate::history::account_of;
use crate::history::authenticated::Index;
use crate::history::authenticated::blobs::{delete_event, write_event};
use crate::history::authenticated::lists::{append, metadata, pop, row};
use crate::history::authenticated::model::{Payload, Queued, Row, event_kind, queue_key};
use crate::history::authenticated::overlay::Overlay;
use crate::history::authenticated::staging::{StageInfo, Staged};
use crate::history::authenticated::state::{CommandId, State, checked_state};
use crate::log::{CommandEntry, Offset, extend_command_prefix};

impl Index {
    /// Ingests parent-authenticated same-prefix input; no pending root is exposed or sealable.
    /// Error cases:
    /// - invalid order/retry, failed proof/stage/durability, or a writer requiring fresh generation
    #[cfg(test)]
    pub fn ingest(&mut self, command: CommandEntry) -> Result<()> {
        self.ingest_batch(std::slice::from_ref(&command))
    }

    /// Ingests consecutive commands as one published update, so a batch costs one durable commit
    /// plus one per full private stage, instead of one per command. Commands the head already
    /// covers are skipped; a retry of the head's command must match it exactly.
    /// Error cases:
    /// - invalid order/retry, failed proof/stage/durability, or a writer requiring fresh generation,
    ///   for any command in the batch; the published head is unchanged on error
    pub fn ingest_batch(&mut self, commands: &[CommandEntry]) -> Result<()> {
        self.ingest_with(commands, &mut |_, _, _| Ok(()))
    }

    /// Drives the real staged pipeline with an observation/failure seam after private durable commits.
    /// Error cases:
    /// - the same validation/write failures as ingest, or the private test observer
    fn ingest_with(
        &mut self,
        commands: &[CommandEntry],
        observe: &mut impl FnMut(&Index, &Root, StageInfo) -> Result<()>,
    ) -> Result<()> {
        ensure!(!self.requires_rebuild, "history writer requires a fresh verified generation");
        let state = match checked_state(&self.store, &self.head) {
            Ok(state) => state,
            Err(error) => {
                self.requires_rebuild = true;
                return Err(error);
            }
        };
        let mut last = state.last_command;
        let mut fresh = Vec::with_capacity(commands.len());
        for command in commands {
            if last.is_some_and(|last| command.sequenced.seq < last.seq) {
                // The parent has already authenticated the same exact prefix; expired payloads are not replay authority.
                continue;
            }
            let identity = CommandId::of(command)?;
            if let Some(last) = last {
                if identity.seq == last.seq {
                    ensure!(identity == last, "conflicting retry at history tip");
                    continue;
                }
                ensure!(last.seq.checked_add(1) == Some(identity.seq), "gap in authenticated history commands");
                ensure!(identity.ts >= last.ts, "reversed history command time");
            } else {
                ensure!(identity.seq == 0, "fresh history must start at command zero");
            }
            ensure!(identity.ts >= 0, "negative history command time");
            last = Some(identity);
            fresh.push((command, identity));
        }
        if fresh.is_empty() {
            return Ok(());
        }

        let result = stage_commands(self, &fresh, state, observe).and_then(|(root, state)| {
            let pin = self.store.lease(&root)?;
            let head = state.head(root)?;
            checked_state(&self.store, &head)?;
            Ok((head, pin))
        });
        match result {
            Ok((head, pin)) => {
                self.head = head;
                self._visible_pin = pin;
                Ok(())
            }
            Err(error) => {
                // Even acknowledged private stages cannot be retried from the older public root.
                self.requires_rebuild = true;
                Err(error)
            }
        }
    }

    /// Observes actual private commits while the public Index remains queryable at its old head.
    /// Error cases:
    /// - actual ingest or observer failure
    #[cfg(test)]
    pub(super) fn ingest_observed(
        &mut self,
        command: CommandEntry,
        mut observe: impl FnMut(&Index, &Root, StageInfo) -> Result<()>,
    ) -> Result<()> {
        self.ingest_with(std::slice::from_ref(&command), &mut observe)
    }
}

/// Executes unbounded logical work using finite overlays, then writes the only complete state.
/// Each command expires and appends against the state the commands before it left.
/// Error cases:
/// - identity/counter representation, serialization, proof, private stage or final durability failure
fn stage_commands(
    index: &Index,
    commands: &[(&CommandEntry, CommandId)],
    old: State,
    observe: &mut impl FnMut(&Index, &Root, StageInfo) -> Result<()>,
) -> Result<(Root, State)> {
    let mut stage = Staged::new(&index.store, &index.head.root, old)?;
    let mut notify = |root: &Root, info: StageInfo| observe(index, root, info);
    for (command, identity) in commands {
        let before = stage.state.clone();
        stage.state.last_command = Some(*identity);
        stage.state.command_prefix = extend_command_prefix(before.command_prefix, identity.digest);
        expire(&mut stage, &before, &mut notify)?;
        for (ordinal, event) in command.events.iter().flatten().enumerate() {
            let Some(account) = account_of(event) else {
                continue;
            };
            let mut value = Row {
                account,
                kind: event_kind(event)?,
                cursor: Offset {
                    seq: identity.seq,
                    index: u32::try_from(ordinal).context("history event cursor overflow")?,
                },
                ts: identity.ts,
                payload: Payload { length: 0, chunks: 0 },
            };
            value.payload = write_event(&mut stage, &value, event, &mut notify)?;
            let all = append(&mut stage.overlay, value.all_list(), &value, &mut stage.state)?;
            let kind = append(&mut stage.overlay, value.kind_list(), &value, &mut stage.state)?;
            let key = queue_key(stage.state.queue_hi);
            ensure!(stage.overlay.read::<Queued>(&key)?.is_none(), "new history queue ordinal is occupied");
            stage.overlay.put(key, &Queued { row: value, all, kind })?;
            stage.state.queue_hi = stage.state.queue_hi.checked_add(1).context("history queue overflow")?;
            stage.flush_if_full(&mut notify)?;
        }
    }
    stage.finish()
}

/// Proves queue identity and both dense references before trusting one expiry decision.
/// Error cases:
/// - missing/invalid queue, metadata, generation, rank or header
fn queued(overlay: &mut Overlay, old: &State, ordinal: u64) -> Result<Queued> {
    ensure!(old.queue_lo <= ordinal && ordinal < old.queue_hi, "queue ordinal outside old interval");
    let item: Queued = overlay.read(&queue_key(ordinal))?.context("missing chronological history queue item")?;
    item.row.validate(item.row.all_list(), old)?;
    ensure!(item.all.generation != item.kind.generation, "history lists share a generation");
    for (list, position) in [(item.row.all_list(), item.all), (item.row.kind_list(), item.kind)] {
        let meta = metadata(overlay, list, old)?.context("queue references absent history metadata")?;
        ensure!(meta.generation == position.generation, "history queue/list generation mismatch");
        ensure!(row(overlay, list, &meta, position.rank, old)? == item.row, "history queue/list header mismatch");
    }
    Ok(item)
}

/// Deletes every strictly expired event and every payload piece, retaining exact horizon equality.
/// Error cases:
/// - inconsistent queue/order/list/payload, overflow, failed proof or private commit
fn expire(stage: &mut Staged, old: &State, observe: &mut impl FnMut(&Root, StageInfo) -> Result<()>) -> Result<()> {
    let horizon = stage.state.horizon();
    let mut previous: Option<Row> = None;
    while stage.state.queue_lo < stage.state.queue_hi {
        let item = queued(&mut stage.overlay, old, stage.state.queue_lo)?;
        if let Some(previous) = &previous {
            ensure!(previous.cursor < item.row.cursor && previous.ts <= item.row.ts, "unordered history queue");
        }
        if item.row.ts >= horizon {
            break;
        }
        delete_event(stage, &item.row, observe)?;
        ensure!(
            stage.overlay.read::<Queued>(&queue_key(stage.state.queue_lo))?.as_ref() == Some(&item),
            "expiring history queue header changed"
        );
        pop(&mut stage.overlay, item.row.all_list(), item.all, &item.row, old)?;
        pop(&mut stage.overlay, item.row.kind_list(), item.kind, &item.row, old)?;
        stage.overlay.delete(queue_key(stage.state.queue_lo))?;
        stage.state.queue_lo = stage.state.queue_lo.checked_add(1).context("history queue overflow")?;
        previous = Some(item.row);
        stage.flush_if_full(observe)?;
    }
    Ok(())
}
