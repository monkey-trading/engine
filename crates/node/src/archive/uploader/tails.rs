//! The tails of the checkpoints the vault may still hold as its latest, through which the bucket alone holds the log up
//! to the vault's latest checkpoint. A range goes up only below the vault's finalized checkpoint, and the vault's latest
//! is a finality delay past it at most times: a paced checkpoint is signed only once its parent is final, and a
//! handover's, a shutdown's or a protocol activation's checkpoint may come between them. So the vault's latest is one of
//! the checkpoints the copy holds from the finalized one on, and the ranges past the finalized one are not in the
//! bucket yet. A tail holds them: its pieces, shaped and named as ranges under a key named for the checkpoint's seq and
//! root (`tail_prefix`), run from the first range the bucket lacks to the checkpoint's own, which ends with the
//! checkpoint's entry. A reader takes the ranges until the first the bucket lacks, and the tail's pieces from there.
//!
//! The uploader reads the copy's entries as its writer publishes them, from the first range the bucket lacks when it
//! starts, and queues the tail of each checkpoint it finds at once, before the vault takes it: the root in the key keeps
//! a checkpoint the vault never takes, as a superseded copy's, from every key a reader derives from the vault. A tail
//! leaves the queue once it is confirmed, or once its checkpoint is older than the vault's finalized one that the copy
//! holds. Each piece is uploaded create-only and read back as a range is; a piece that reads back otherwise pages
//! `archive_object_mismatch`, and that tail goes no further. A copy stalled at a range, or superseded, uploads none.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Mutex, PoisonError};

use anyhow::Context;
use engine_core::eip712::B256;
use tokio::task::spawn_blocking;
use tokio::time::Instant;
use tracing::{Instrument, Span, info, info_span, warn};

use crate::archive::uploader::{Found, Source, Work};
use crate::archive::{Finality, Store, piece_bytes, range_key, range_seqs, tail_prefix};
use crate::log::{Checkpoint, LogEntry, LogReader, Manifest, Offset};

/// How many confirmed tails are remembered, so another copy of the same history does not upload them again.
const DONE: usize = 16;

/// The tails' work across sources.
#[derive(Default)]
pub(super) struct Tails {
    /// The reader of the copy in use, for checkpoints; none before the first step reads, and after a change of source.
    /// Behind a lock only so the uploader's future can be sent: the reader leaves it by value, for a blocking read.
    scan: Option<Mutex<LogReader>>,
    /// Where the writer's flushed entries stood when the reader last caught up with them.
    scanned: Option<Offset>,
    /// The tails waiting, oldest first.
    queue: VecDeque<Tail>,
    /// The newest checkpoint found in the copy in use, so a reread queues none twice.
    newest: Option<u64>,
    /// The tails confirmed, newest last.
    done: VecDeque<(u64, B256)>,
    /// The pieces already reported as mismatched.
    mismatched: BTreeSet<String>,
    /// Since when a tail has waited with none confirmed, or the copy could not be read for its checkpoints, whatever
    /// tails have come and gone and whatever source is in use: what `archive_upload_lagging` measures for the tails.
    since: Option<Instant>,
    /// The last read of the copy for checkpoints failed: the checkpoints it would have found wait too.
    unread: bool,
}

/// A checkpoint's tail, and how far its upload has come.
struct Tail {
    seq: u64,
    root: B256,
    /// The first piece not yet confirmed; none below the first range the bucket lacks goes up.
    next: u64,
}

impl<F: Finality> Work<F> {
    /// Starts the tails afresh for another copy, which may hold another history: its checkpoints are read from the
    /// first range the bucket lacks. A wait already under way goes on: the tails the copy queues wait from its start.
    pub(super) fn restart_tails(&mut self) {
        let (done, mismatched) = (std::mem::take(&mut self.tails.done), std::mem::take(&mut self.tails.mismatched));
        let (since, unread) = (self.tails.since, self.tails.unread);
        self.tails = Tails { done, mismatched, since, unread, ..Tails::default() };
    }

    /// One step of the tails' work while no range can go up: reads the entries the writer has flushed since the last
    /// step for checkpoints, then uploads the next piece of the oldest tail waiting, and returns how that went, with the
    /// span to report a failure in; `None` when no tail waits. A copy stalled at a range, or superseded, uploads none.
    /// Error cases:
    /// - the copy cannot be read
    /// - a piece cannot be built, or the bucket cannot take or serve it
    pub(super) async fn tail_step(&mut self, manifest: &Source) -> Option<(anyhow::Result<()>, Span)> {
        if self.stalled_at.is_some() || self.anchor.differs.is_some() {
            self.tails.queue.clear();
            self.tails.unread = false;
            self.waiting();
            return None;
        }
        let flushed = manifest.borrow().flushed;
        if flushed.is_some()
            && flushed != self.tails.scanned
            && let Err(e) = self.scan_tails(manifest, flushed).await
        {
            self.waiting();
            return Some((Err(e), Span::none()));
        }
        // The vault's latest checkpoint is never older than its finalized one.
        if let Some((finalized, _)) = &self.anchor.held {
            self.tails.queue.retain(|tail| tail.seq >= *finalized);
        }
        self.waiting();
        self.tails.queue.front()?;
        let view = manifest.borrow().clone();
        Some(self.piece(&view).await)
    }

    /// Reads the copy's entries up to the writer's flushed one for checkpoints, from where the last read stopped, or
    /// from the first range the bucket lacks, and queues each new one's tail; `flushed` is where the writer stood.
    /// Error cases:
    /// - the copy cannot be read
    async fn scan_tails(&mut self, manifest: &Source, flushed: Option<Offset>) -> anyhow::Result<()> {
        self.tails.unread = true;
        let held = self.tails.scan.take().map(|held| held.into_inner().unwrap_or_else(PoisonError::into_inner));
        let (dir, manifest) = (self.dir.clone(), manifest.clone());
        let from = Offset { seq: self.backlog.next * range_seqs(), index: 0 };
        let (reader, found) = spawn_blocking(move || {
            let mut reader = match held {
                Some(reader) => reader,
                None => match LogReader::open(&dir, from, manifest) {
                    Ok(reader) => reader,
                    Err(e) => return (None, Err(e)),
                },
            };
            let found = checkpoints(&mut reader);
            (Some(reader), found)
        })
        .await
        .context("reading the copy for checkpoints stopped")?;
        // A failed read may have passed checkpoints it does not return: the next starts again from the first range
        // the bucket lacks, and queues only those newer than the last it queued.
        self.tails.scan = reader.filter(|_| found.is_ok()).map(Mutex::new);
        self.tails.unread = found.is_err();
        for checkpoint in found.context("reading the copy for checkpoints")? {
            self.found(checkpoint);
        }
        self.waiting();
        self.tails.scanned = flushed;
        Ok(())
    }

    /// Queues the tail of `checkpoint`, found in the copy, unless it is not newer than the last found or is
    /// confirmed. A root that does not parse names no key, and is passed over: no vault takes it.
    fn found(&mut self, checkpoint: Checkpoint) {
        let seq = checkpoint.seq;
        if self.tails.newest.is_some_and(|newest| seq <= newest) {
            return;
        }
        self.tails.newest = Some(seq);
        let Ok(root) = checkpoint.root.parse::<B256>() else {
            warn!(seq, root = %checkpoint.root, "the copy's checkpoint names a root that is not 32 bytes, so it has no tail");
            return;
        };
        if !self.tails.done.contains(&(seq, root)) {
            self.tails.queue.push_back(Tail { seq, root, next: 0 });
        }
    }

    /// Uploads the next piece of the oldest tail waiting, from `view`'s copy, and reads it back, in the piece's span,
    /// which it returns; the tail is confirmed once its checkpoint's piece is, or at once when the bucket's ranges
    /// already hold the checkpoint's range.
    /// Error cases:
    /// - the piece cannot be built, or the bucket cannot take or serve it
    async fn piece(&mut self, view: &Manifest) -> (anyhow::Result<()>, Span) {
        let tail = self.tails.queue.front().expect("a tail waits: the caller checked");
        let (seq, root, last) = (tail.seq, tail.root, tail.seq / range_seqs());
        let index = tail.next.max(self.backlog.next);
        if index > last {
            self.confirmed();
            return (Ok(()), Span::none());
        }
        // The tail's own store: its pieces are plain names directly under the tail's prefix, and nowhere else. Its seq
        // and root are digits and hex, so the tail's prefix is always a directory below the log's.
        let store = self.store.within(&tail_prefix(self.store.prefix(), seq, &root)).expect("a tail's directory");
        let key = format!("{}{}", store.prefix(), range_key("", index));
        let span = info_span!("archive_tail", id = %key);
        let result = self.upload_piece(view, (seq, index), (&store, &key)).instrument(span.clone()).await;
        (result, span)
    }

    /// Uploads piece `index` of the oldest tail waiting, the checkpoint's at `seq`, at `key` of the tail's `store`, and
    /// reads it back.
    /// Error cases:
    /// - the piece cannot be built, or the bucket cannot take or serve it
    async fn upload_piece(
        &mut self,
        view: &Manifest,
        (seq, index): (u64, u64),
        (store, key): (&Store, &str),
    ) -> anyhow::Result<()> {
        let (dir, read) = (self.dir.clone(), view.clone());
        let raw = std::sync::Arc::new(spawn_blocking(move || piece_bytes(&dir, &read, index, seq)).await??);
        let (created, stored) = self.create(store, key, &raw).await?;
        let bytes = raw.len() as u64;
        match self.found_at(store, key, raw).await? {
            Found::Confirmed => {
                info!(?created, bytes, stored, "confirmed in the archive");
                match index == seq / range_seqs() {
                    true => self.confirmed(),
                    false => self.tails.queue.front_mut().expect("the tail uploaded").next = index + 1,
                }
            }
            Found::Mismatched => {
                self.piece_mismatch(key, index);
                self.tails.queue.pop_front();
                self.waiting();
            }
        }
        Ok(())
    }

    /// Hands the backlog the oldest tail waiting, or none while the copy cannot be read, and since when a tail has
    /// waited with none confirmed, which counts toward `archive_upload_lagging`; with nothing waiting, the wait ends.
    fn waiting(&mut self) {
        let front = self.tails.queue.front().map(|tail| tail.seq);
        if front.is_none() && !self.tails.unread {
            (self.tails.since, self.backlog.tail) = (None, None);
            return;
        }
        let since = *self.tails.since.get_or_insert_with(Instant::now);
        self.backlog.tail = Some((front, since));
    }

    /// Records the oldest tail waiting as confirmed: the tails still waiting wait from now.
    fn confirmed(&mut self) {
        let tail = self.tails.queue.pop_front().expect("a tail waits: the caller checked");
        self.tails.since = None;
        self.waiting();
        self.tails.done.push_back((tail.seq, tail.root));
        if self.tails.done.len() > DONE {
            self.tails.done.pop_front();
        }
        let gauge = &self.metrics.archive.tail_seq;
        gauge.set(gauge.get().max(i64::try_from(tail.seq).unwrap_or(i64::MAX)));
        info!(seq = tail.seq, root = %tail.root, "the archive's bucket holds the log through this checkpoint's tail");
    }

    /// Reports the piece at `key`, of range `index`, as other than the node's copy, once; its tail goes no further.
    fn piece_mismatch(&mut self, key: &str, index: u64) {
        self.metrics.archive.mismatches.inc();
        if self.tails.mismatched.insert(key.to_owned()) {
            warn!(
                alert = "archive_object_mismatch",
                object = %key,
                first_seq = index * range_seqs(),
                "the archive's object for this piece of a checkpoint's tail differs from this node's copy of it and \
                 cannot be replaced, so this node uploads no more of that tail: compare the two"
            );
        }
    }
}

/// The checkpoints `reader` reads up to its writer's flushed entry, in log order.
/// Error cases:
/// - an entry fails its check against the writer's listing or prefix, or its segment is not on this host
fn checkpoints(reader: &mut LogReader) -> anyhow::Result<Vec<Checkpoint>> {
    let mut found = Vec::new();
    while let Some((_, entry)) = reader.next_entry()? {
        if let LogEntry::Checkpoint(checkpoint) = entry {
            found.push(checkpoint);
        }
    }
    reader.release();
    Ok(found)
}
