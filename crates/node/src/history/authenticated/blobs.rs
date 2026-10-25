//! Authenticated bounded event chunks; only complete reconstructed payloads reach the API.

use std::io::{self, Write};

use anyhow::{Context, Result, ensure};
use engine_core::event::Event;

use crate::authenticated::{Root, Store};
use crate::history::account_of;
use crate::history::authenticated::model::{PAYLOAD_CHUNK_BYTES, Payload, Row, event_kind};
use crate::history::authenticated::staging::{StageInfo, Staged};

/// Serializes directly into bounded private chunks, without a second full event allocation.
/// Error cases:
/// - serialization, representational overflow, duplicate chunk, proof or private commit failure
pub(super) fn write_event(
    stage: &mut Staged,
    row: &Row,
    event: &Event,
    observe: &mut impl FnMut(&Root, StageInfo) -> Result<()>,
) -> Result<Payload> {
    let mut writer =
        ChunkWriter { stage, row, observe, buffer: Vec::with_capacity(PAYLOAD_CHUNK_BYTES), length: 0, chunks: 0 };
    serde_json::to_writer(&mut writer, event).context("streaming history event")?;
    writer.finish()
}

/// Proves every expected piece under the same root before parsing the complete public event.
/// Error cases:
/// - missing/wrong-sized chunk, allocation/representation failure, invalid event or scope mismatch
pub(super) fn read_event(store: &Store, root: &Root, row: &Row) -> Result<Event> {
    row.payload.validate()?;
    let length = usize::try_from(row.payload.length).context("history event exceeds local representation")?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).context("allocating returned history event")?;
    for ordinal in 0..row.payload.chunks {
        let chunk = store.read(root, &row.chunk_key(ordinal))?.context("missing history payload chunk")?;
        ensure!(chunk.len() == row.payload.chunk_len(ordinal)?, "wrong history payload chunk length");
        bytes.extend_from_slice(&chunk);
    }
    ensure!(bytes.len() == length, "history payload total length mismatch");
    let event: Event = serde_json::from_slice(&bytes).context("decoding complete history event")?;
    ensure!(account_of(&event) == Some(row.account), "history payload account mismatch");
    ensure!(event_kind(&event)? == row.kind, "history payload kind mismatch");
    Ok(event)
}

/// Proves and removes all pieces in finite overlays before expiry can remove their list/queue header.
/// Error cases:
/// - invalid descriptor, missing/wrong-sized chunk, proof or staged durable commit failure
pub(super) fn delete_event(
    stage: &mut Staged,
    row: &Row,
    observe: &mut impl FnMut(&Root, StageInfo) -> Result<()>,
) -> Result<()> {
    row.payload.validate()?;
    for ordinal in 0..row.payload.chunks {
        let key = row.chunk_key(ordinal);
        let bytes = stage.overlay.read_bytes(&key)?.context("missing expiring history payload chunk")?;
        ensure!(bytes.len() == row.payload.chunk_len(ordinal)?, "wrong expiring payload chunk length");
        stage.overlay.delete(key)?;
        stage.flush_if_full(observe)?;
    }
    Ok(())
}

struct ChunkWriter<'a, F> {
    stage: &'a mut Staged,
    row: &'a Row,
    observe: &'a mut F,
    buffer: Vec<u8>,
    length: u64,
    chunks: u64,
}

impl<F: FnMut(&Root, StageInfo) -> Result<()>> Write for ChunkWriter<'_, F> {
    /// Accepts arbitrarily large serializer slices while retaining at most one chunk locally.
    /// Error cases:
    /// - checked length overflow, proof/write failure, or allocation failure
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let size = bytes.len();
        let extra = u64::try_from(size).map_err(io::Error::other)?;
        self.length = self.length.checked_add(extra).ok_or_else(|| io::Error::other("payload length overflow"))?;
        while !bytes.is_empty() {
            let take = bytes.len().min(PAYLOAD_CHUNK_BYTES - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == PAYLOAD_CHUNK_BYTES {
                self.emit().map_err(io::Error::other)?;
            }
        }
        Ok(size)
    }

    /// Keeps a partial chunk buffered; only finish may emit a short final chunk.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<F: FnMut(&Root, StageInfo) -> Result<()>> ChunkWriter<'_, F> {
    /// Emits the last piece and returns the exact authenticated descriptor.
    /// Error cases:
    /// - any final chunk/proof/commit failure or invalid resulting geometry
    fn finish(mut self) -> Result<Payload> {
        if !self.buffer.is_empty() {
            self.emit()?;
        }
        let payload = Payload { length: self.length, chunks: self.chunks };
        payload.validate()?;
        Ok(payload)
    }

    /// Stages a uniquely scoped piece and flushes the bounded overlay when full.
    /// Error cases:
    /// - old-key proof/nonmembership, counter overflow, or staged durable write failure
    fn emit(&mut self) -> Result<()> {
        let key = self.row.chunk_key(self.chunks);
        ensure!(self.stage.overlay.read_bytes(&key)?.is_none(), "new history payload chunk is occupied");
        let bytes = std::mem::replace(&mut self.buffer, Vec::with_capacity(PAYLOAD_CHUNK_BYTES));
        self.stage.overlay.put_bytes(key, bytes)?;
        self.chunks = self.chunks.checked_add(1).context("history chunk ordinal overflow")?;
        self.stage.flush_if_full(self.observe)
    }
}
