//! The rows a command derives and the log does not carry (`Engine::take_derived`): from `V80` a
//! rollover's `financing` row per position, which its logged `financing_applied` covers. The newest
//! commands' rows stay in memory (`DerivedRows`): the sequencer keeps each command's as it applies it,
//! recovery's replay seeds them, the history index takes a command's rows from them, and the financing
//! read proves an account's charges against the logged root from them. They are kept for a week of
//! rollovers, so an index that falls behind by up to that, as a rotation member's does between its
//! turns, or one that restarts, finds them in memory. The history writes each command's rows to disk as
//! it takes them (`RowsStore`), for its retention, so the financing read proves an older row from there.
//! A command the index reads back from the log once its rows are gone takes them from that store, or
//! else from a replay of the log of its own (`Deriver`), which a stop ends and which pages a Warning.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, bail, ensure};
use engine_core::Engine;
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::financing::{ChargesTree, FinancingRow};
use engine_core::types::Millis;
use prometheus_client::metrics::counter::Counter;
use tokio::sync::watch;
use tracing::warn;

use crate::log::{CommandEntry, LogEntry, LogReader, Manifest, Offset};

mod store;
pub(crate) use store::RowsStore;

/// How many commands' derived rows stay in memory: a week of daily rollovers, about 21 MB each at the
/// launch book's 440k positions.
pub(crate) const RETAINED: usize = 8;

/// One command's derived rows, shared without a copy.
pub(crate) type Rows = Arc<Vec<FinancingRow>>;

/// One rollover's `financing_applied` in its command's logged events, and where its rows sit among the
/// command's derived rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Aggregate {
    /// The event's index among the command's logged events.
    pub index: u32,
    pub at: Millis,
    pub root: B256,
    /// Its rows: those from `first`, `positions` of them.
    pub first: usize,
    pub positions: usize,
}

/// One command's derived rows, and the aggregates that cover them, in turn.
#[derive(Clone, Debug)]
pub(crate) struct Derived {
    pub aggregates: Vec<Aggregate>,
    pub rows: Rows,
    /// Each aggregate's tree over its rows, built by the first read that needs them.
    trees: Arc<OnceLock<Vec<ChargesTree>>>,
}

/// The newest `RETAINED` commands' derived rows, by seq.
#[derive(Clone, Debug, Default)]
pub(crate) struct DerivedRows(BTreeMap<u64, Derived>);

/// A command the history index takes, with its derived rows.
#[derive(Clone, Debug)]
pub(crate) struct Taken {
    pub command: CommandEntry,
    pub derived: Option<Rows>,
}

/// Finds the derived rows of each command the history index takes: from the rows in memory while they
/// hold the command's, and otherwise from the rows stored on disk, or a replay of the log from its
/// start, kept while the index reads commands whose rows are gone. It stores each command's rows.
pub(crate) struct Deriver {
    kept: watch::Receiver<DerivedRows>,
    /// The log a replay reads, and its writer's published frontier; none where every command's rows
    /// stay in memory until the index takes them.
    log: Option<(PathBuf, watch::Receiver<Manifest>)>,
    replay: Option<Replay>,
    /// Set when the history stops, which ends a replay between two entries.
    stop: Arc<AtomicBool>,
    /// Where each command's rows are written as the index takes them, and read once memory lacks them.
    store: Option<Arc<RowsStore>>,
    /// Counts each replay from the log's start (`full_replays`).
    replays: Counter,
}

/// Sets its flag when dropped: a history task that ends, or is aborted, stops its deriver's replay.
pub(crate) struct StopOnDrop(pub Arc<AtomicBool>);

/// A replay of the log from its start, applying each command to an engine of its own.
struct Replay {
    reader: LogReader,
    engine: Engine,
}

impl Drop for StopOnDrop {
    /// Sets the flag.
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Derived {
    /// `rows`, the derived rows of the command at `seq`, whose logged `events` cover them; none when
    /// there are none.
    /// Error cases:
    /// - the rows are not those the events' aggregates cover, which means the engine broke its contract
    pub(crate) fn of(seq: u64, events: &[Event], rows: Vec<FinancingRow>) -> anyhow::Result<Option<Self>> {
        let aggregates = aggregates(events)?;
        let covered: usize = aggregates.iter().map(|aggregate| aggregate.positions).sum();
        ensure!(covered == rows.len(), "seq {seq} derived {} rows, not the {covered} its aggregates cover", rows.len());
        Ok((!rows.is_empty()).then(|| Self { aggregates, rows: Arc::new(rows), trees: Arc::default() }))
    }

    /// The rows `aggregate`, one of these, covers.
    pub(crate) fn rows_of(&self, aggregate: &Aggregate) -> &[FinancingRow] {
        &self.rows[aggregate.first..aggregate.first + aggregate.positions]
    }

    /// `of`, for rows from where the host can change them: each aggregate's tree over its rows gives
    /// the root it logs.
    /// Error cases:
    /// - as for `of`, or a tree's root is not its aggregate's
    pub(crate) fn checked(seq: u64, events: &[Event], rows: Vec<FinancingRow>) -> anyhow::Result<Option<Self>> {
        let Some(derived) = Self::of(seq, events, rows)? else { return Ok(None) };
        for (aggregate, tree) in derived.aggregates.iter().zip(derived.trees()) {
            ensure!(tree.root() == aggregate.root, "seq {seq}'s stored rows miss the root at {}", aggregate.index);
        }
        Ok(Some(derived))
    }

    /// Each aggregate's tree over its rows, in turn, built on the first call: a pass over every row.
    pub(crate) fn trees(&self) -> &[ChargesTree] {
        self.trees
            .get_or_init(|| self.aggregates.iter().map(|aggregate| ChargesTree::new(self.rows_of(aggregate))).collect())
    }
}

impl DerivedRows {
    /// Keeps `derived`, the command at `seq`'s, dropping the oldest past `RETAINED`.
    pub(crate) fn keep(&mut self, seq: u64, derived: Derived) {
        self.0.insert(seq, derived);
        while self.0.len() > RETAINED {
            self.0.pop_first();
        }
    }

    /// The derived rows of the command at `seq`, if kept.
    pub(crate) fn get(&self, seq: u64) -> Option<&Derived> {
        self.0.get(&seq)
    }
}

impl Deriver {
    /// Takes rows from `kept`, and from a replay of the log in `log` once `kept` no longer holds them.
    pub(crate) fn new(kept: watch::Receiver<DerivedRows>, log: Option<(PathBuf, watch::Receiver<Manifest>)>) -> Self {
        Self { kept, log, replay: None, stop: Arc::default(), store: None, replays: Counter::default() }
    }

    /// This deriver, its replay ended once `stop` is set.
    pub(crate) fn stopped_by(self, stop: Arc<AtomicBool>) -> Self {
        Self { stop, ..self }
    }

    /// This deriver, storing each command's rows in `store` and counting each replay in `replays`.
    pub(crate) fn storing(self, store: Arc<RowsStore>, replays: Counter) -> Self {
        Self { store: Some(store), replays, ..self }
    }

    /// A deriver for commands that derive no rows, which keeps none and reads no log.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self::new(watch::channel(DerivedRows::default()).1, None)
    }

    /// The derived rows of `command`, which the log holds and the history index takes next, written to
    /// the store: none when its logged events cover none. Rows gone from memory come from the store
    /// when its file gives the logged aggregates, and otherwise from a replay, an engine of its own,
    /// kept only while the index takes commands older than the oldest whose rows are in memory.
    /// Error cases:
    /// - its rows are gone from memory and the store, and there is no log to replay, or the replay fails
    ///   or is stopped (`Replay::rows`)
    /// - the rows found are not those its logged aggregates cover
    pub(crate) fn rows(&mut self, command: &CommandEntry) -> anyhow::Result<Option<Rows>> {
        let seq = command.sequenced.seq;
        let (kept, oldest) = {
            let kept = self.kept.borrow();
            (kept.get(seq).map(|derived| derived.rows.clone()), kept.0.keys().next().copied())
        };
        if oldest.is_some_and(|oldest| seq >= oldest) {
            // Every later command's rows are in memory, unless the index falls behind again.
            self.replay = None;
        }
        let events = command.events.as_deref().unwrap_or_default();
        let aggregates = aggregates(events)?;
        let expected: usize = aggregates.iter().map(|aggregate| aggregate.positions).sum();
        if expected == 0 {
            return Ok(None);
        }
        let rows = match kept {
            Some(rows) => rows,
            None => match self.stored(command) {
                Some(rows) => return Ok(Some(rows)),
                None => Arc::new(self.replayed(command)?),
            },
        };
        ensure!(
            rows.len() == expected,
            "seq {seq} has {} derived rows, not the {expected} its aggregates cover",
            rows.len()
        );
        if let Some(store) = &self.store
            && let Err(e) = store.record(seq, command.sequenced.ts, &rows)
        {
            warn!(alert = "financing_rows_unstored", seq, "the financing read cannot prove these rows: {e:#}");
        }
        Ok(Some(rows))
    }

    /// `command` with its derived rows (`rows`), for the authenticated history's batch; none when the
    /// history's stop ended a replay of the log, which leaves the command to the next start.
    /// Error cases:
    /// - as for `rows`, unless the history stopped
    pub(crate) fn taken(&mut self, command: &CommandEntry) -> anyhow::Result<Option<Taken>> {
        match self.rows(command) {
            Ok(derived) => Ok(Some(Taken { derived, command: command.clone() })),
            Err(_) if self.stop.load(Ordering::Acquire) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `command`'s rows from the store, when its file gives the aggregates `command` logs.
    fn stored(&self, command: &CommandEntry) -> Option<Rows> {
        match self.store.as_ref()?.derived(command) {
            Ok(found) => found.map(|derived| derived.rows),
            Err(e) => {
                let seq = command.sequenced.seq;
                warn!(seq, "the stored rows are not the logged ones, so the log is replayed: {e:#}");
                None
            }
        }
    }

    /// `command`'s rows from a replay of the log, opened from its start unless the one kept is behind it.
    /// Error cases:
    /// - there is no log to replay, or the replay fails or is stopped (`Replay::rows`)
    fn replayed(&mut self, command: &CommandEntry) -> anyhow::Result<Vec<FinancingRow>> {
        let seq = command.sequenced.seq;
        let (dir, manifest) = self.log.as_ref().with_context(|| format!("no rows kept for seq {seq}"))?;
        let replay = match self.replay.take() {
            Some(replay) if replay.engine.last_seq().is_none_or(|last| last < seq) => replay,
            _ => {
                warn!(alert = "history_rows_replayed", seq, "re-deriving a command's rows by replaying the log");
                self.replays.inc();
                let reader = LogReader::open(dir, Offset::start_of(0), manifest.clone())?;
                Replay { reader, engine: Engine::new() }
            }
        };
        self.replay.insert(replay).rows(command, &self.stop)
    }

    /// How many changed keys the replay's engine holds, if one is kept.
    #[cfg(test)]
    pub(crate) fn replay_changes(&self) -> Option<usize> {
        self.replay.as_ref().map(|replay| replay.engine.changed_key_count())
    }
}

impl Replay {
    /// Applies the log's commands up to `command`'s, and returns the rows it derives.
    /// Error cases:
    /// - the log cannot be read, or ends before `command`
    /// - a command does not apply, or its logged events differ from what the engine gives
    /// - the log holds another command at `command`'s seq
    /// - `stop` is set
    fn rows(&mut self, command: &CommandEntry, stop: &AtomicBool) -> anyhow::Result<Vec<FinancingRow>> {
        let seq = command.sequenced.seq;
        while let Some((_, entry)) = self.reader.next_entry()? {
            ensure!(!stop.load(Ordering::Acquire), "history stopped while re-deriving the rows of seq {seq}");
            let LogEntry::Command(logged) = entry else { continue };
            let at = logged.sequenced.seq;
            let events = self.engine.apply(&logged.sequenced).with_context(|| format!("re-deriving at seq {at}"))?;
            // The replay never computes a root, the only thing that drains the engine's record of
            // changed keys, so it is dropped here or it grows with every order the log ever held.
            self.engine.discard_changed_keys();
            ensure!(
                logged.events.as_ref().is_none_or(|logged| *logged == events),
                "events logged at seq {at} differ from the events replay gives"
            );
            if at == seq {
                ensure!(logged == *command, "the log holds another command at seq {seq}");
                return Ok(self.engine.take_derived());
            }
        }
        bail!("the log ends before seq {seq}, whose derived rows the history index takes")
    }
}

/// The events the history index stores for `command`, in order: its logged events, then its derived
/// rows as the `financing` events they stand for, so a derived row's index among them is past every
/// logged event's.
pub(crate) fn indexed<'a>(
    command: &'a CommandEntry,
    derived: Option<&'a Rows>,
) -> impl Iterator<Item = Cow<'a, Event>> {
    let rows = derived.into_iter().flat_map(|rows| rows.iter().map(|row| Cow::Owned(row.event())));
    command.events.iter().flatten().map(Cow::Borrowed).chain(rows)
}

/// The aggregates among `events`, one command's logged events, with where their rows sit.
/// Error cases:
/// - a count does not fit this machine's words
fn aggregates(events: &[Event]) -> anyhow::Result<Vec<Aggregate>> {
    let mut first = 0;
    let mut out = Vec::new();
    for (index, event) in events.iter().enumerate() {
        let Event::FinancingApplied { at, root, positions, .. } = event else { continue };
        let positions = usize::try_from(*positions).context("aggregate positions")?;
        let index = u32::try_from(index).context("aggregate index")?;
        out.push(Aggregate { index, at: *at, root: *root, first, positions });
        first += positions;
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
