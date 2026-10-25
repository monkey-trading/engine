//! Checks the bytes actually returned to decoders, including bounded active-file prefixes.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, bail, ensure};
use sha2::{Digest, Sha256};

use crate::log::{MANIFEST, SegmentInfo};

const CHUNK_BYTES: usize = if cfg!(test) { 64 } else { 1 << 20 };

#[derive(Clone)]
pub(super) struct ChunkHashes {
    hashes: Arc<[[u8; 32]]>,
    bytes: u64,
}

pub(super) struct CheckedChunks {
    file: File,
    chunks: ChunkHashes,
    next_chunk: usize,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for CheckedChunks {
    /// Returns only bytes checked in the current chunk's private buffer.
    /// Error cases:
    /// - a needed chunk is missing or differs from its commitment
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pos == self.buf.len() {
            self.load()?;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl CheckedChunks {
    /// Reopens a checked prefix and authenticates its chunk before exposing any bytes.
    /// Error cases:
    /// - position is outside the prefix, IO fails, or the requested chunk changed
    pub(super) fn open(path: &Path, chunks: ChunkHashes, position: u64) -> anyhow::Result<Self> {
        ensure!(position <= chunks.bytes, "position outside checked prefix");
        let first = position / CHUNK_BYTES as u64;
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        file.seek(SeekFrom::Start(first * CHUNK_BYTES as u64))?;
        let next_chunk = usize::try_from(first).context("segment position")?;
        let mut reader = Self { file, chunks, next_chunk, buf: Vec::new(), pos: 0 };
        reader.load()?;
        reader.pos = (position % CHUNK_BYTES as u64) as usize;
        ensure!(reader.pos <= reader.buf.len(), "checked prefix position");
        Ok(reader)
    }

    /// Verifies exactly the committed bytes in the next chunk, ignoring unflushed suffix bytes.
    /// Error cases:
    /// - reading fails, a committed byte is missing, or a digest differs
    fn load(&mut self) -> io::Result<()> {
        self.buf.clear();
        self.pos = 0;
        let Some(expected) = self.chunks.hashes.get(self.next_chunk) else {
            return Ok(());
        };
        let start = self.next_chunk as u64 * CHUNK_BYTES as u64;
        let want = (self.chunks.bytes - start).min(CHUNK_BYTES as u64) as usize;
        self.buf.resize(want, 0);
        self.file.read_exact(&mut self.buf)?;
        if <[u8; 32]>::from(Sha256::digest(&self.buf)) != *expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("chunk {} does not match its listing in {MANIFEST} or its flushed prefix", self.next_chunk),
            ));
        }
        self.next_chunk += 1;
        Ok(())
    }
}

/// Hash state after the whole chunks of a checked prefix, so a check of a longer prefix of the same
/// file hashes only the bytes after them.
#[derive(Clone, Default)]
pub(super) struct Progress {
    whole: Sha256,
    hashes: Vec<[u8; 32]>,
}

impl Progress {
    fn bytes(&self) -> u64 {
        self.hashes.len() as u64 * CHUNK_BYTES as u64
    }
}

/// Checks a complete immutable segment, including its exact file length.
/// Error cases:
/// - bytes, digest, length, or IO differ from its authenticated descriptor
pub(super) fn check(path: &Path, listed: &SegmentInfo) -> anyhow::Result<ChunkHashes> {
    Ok(extend(path, listed, true, None)?.0)
}

/// Checks a writer-committed prefix; subsequent unchecked/unflushed bytes carry no authority.
/// Error cases:
/// - a committed byte is missing, the digest differs, or IO fails
#[cfg(test)]
pub(super) fn check_prefix(path: &Path, prefix: &SegmentInfo) -> anyhow::Result<ChunkHashes> {
    Ok(extend(path, prefix, false, None)?.0)
}

/// Checks `listed` as `check` (`complete`) or `check_prefix` does, hashing from the end of `known`:
/// the progress of an earlier check of the same file at a shorter or equal length. The earlier
/// bytes are not re-read; each chunk read later is still checked against its hash.
/// Error cases:
/// - reading fails, a prefix is truncated, or the descriptor does not match
pub(super) fn extend(
    path: &Path,
    listed: &SegmentInfo,
    complete: bool,
    known: Option<Progress>,
) -> anyhow::Result<(ChunkHashes, Progress)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut progress = known.filter(|known| known.bytes() <= listed.bytes).unwrap_or_default();
    let (mut whole, mut tail, mut bytes) = (progress.whole.clone(), None, progress.bytes());
    file.seek(SeekFrom::Start(bytes))?;
    let mut buf = vec![0; CHUNK_BYTES].into_boxed_slice();
    while bytes < listed.bytes {
        let want = (listed.bytes - bytes).min(CHUNK_BYTES as u64) as usize;
        match file.read_exact(&mut buf[..want]) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                bail!("{} does not match its listing in {MANIFEST} or its flushed prefix (shorter)", path.display())
            }
            Err(e) => return Err(e).with_context(|| format!("reading checked prefix {}", path.display())),
        }
        whole.update(&buf[..want]);
        let hash = Sha256::digest(&buf[..want]).into();
        bytes += want as u64;
        if want == CHUNK_BYTES {
            progress.hashes.push(hash);
            progress.whole = whole.clone();
        } else {
            tail = Some(hash);
        }
    }
    if complete {
        let mut extra = [0];
        ensure!(file.read(&mut extra)? == 0, "{} does not match its listing in {MANIFEST} (length)", path.display());
    }
    ensure!(
        hex::encode(whole.finalize()) == listed.sha256,
        "{} does not match its listing in {MANIFEST} or its flushed prefix",
        path.display()
    );
    let hashes = progress.hashes.iter().copied().chain(tail).collect();
    Ok((ChunkHashes { hashes, bytes }, progress))
}
