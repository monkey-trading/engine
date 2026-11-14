//! Crash-durable nonce ownership for the log-head relayer. Unlike the checkpoint submission store,
//! a reservation here authenticates nothing: a log-head post carries no authority, so a replaced,
//! reverted, or consumed nonce only delays visibility, and the contract accepts any newer head
//! from the signer that extends the head it holds. The store therefore tracks only the durable reservation
//! it must not double-send at a live nonce, never the consumer of a consumed one.
//!
//! All writes are atomic validate-and-replace: a write the caller bounded with a timeout may
//! complete late and overwrite a newer record, and that is harmless — the next read re-derives the
//! same reservation and rewrites it, and a record pairing an older head with a newer signature is
//! rebuilt from what is re-read before anything re-sends.
//!
//! Each operation takes the lock inside its blocking worker and holds it until the operation
//! completes: a caller's timeout abandons the wait, never the lock, so a stalled writer keeps
//! exclusive access — staging file and rename included — and a retry's write queues behind it
//! instead of overlapping it.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use super::Head;

const RECORD_LIMIT: u64 = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reservation {
    version: u8,
    relayer: Address,
    contract: Address,
    chain_id: u64,
    /// The head this nonce sends, if a send has been made at it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<(u64, Head)>,
    pub nonce: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<B256>,
    /// The exact signed call this reservation sends, so a reconciliation rebroadcasts that call at
    /// the same nonce, never a newer head at the old nonce. Epoch, head,
    /// parent, and signature are persisted together, before the broadcast: a crash after the
    /// endpoint received the transaction still leaves the call that may land.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// A v2 head's whole `postV2` call, which a rebroadcast sends as it is; `None` for a v1 head,
    /// whose call is rebuilt from the head, the parent and the signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calldata: Option<alloy::primitives::Bytes>,
    /// The parent head the stored call extends; zeroes for the first post of the signer epoch.
    #[serde(default)]
    pub parent_seq: u64,
    #[serde(default)]
    pub parent_link: B256,
    /// Unix seconds when the reservation last changed hands, persisted for forensics only: the
    /// host writes this file, so the pending wait is never bounded by it (the caller keeps an
    /// in-memory deadline from when this process first saw the send).
    #[serde(default)]
    pub stored_at: u64,
}

impl Reservation {
    /// Records the call this nonce is about to send: epoch, head, and parent together, so the
    /// signature the caller stores with it pairs with them — a later reconciliation rebroadcasts
    /// exactly these bytes, never a newer signature against an older head.
    pub(super) fn record_send(&mut self, epoch: u64, head: Head, parent: Option<Head>) {
        self.head = Some((epoch, head));
        self.parent_seq = parent.map_or(0, |parent| parent.seq);
        self.parent_link = parent.map_or(B256::ZERO, |parent| parent.link);
    }
}

#[derive(Debug, Clone)]
pub(super) struct SubmissionStore {
    path: PathBuf,
    relayer: Address,
    contract: Address,
    chain_id: u64,
}

impl SubmissionStore {
    /// Parses existing storage without creating or changing any file.
    /// Error cases:
    /// - an existing reservation is unreadable, malformed, or belongs to another deployment
    pub fn pending(dir: &Path, relayer: Address, contract: Address, chain_id: u64) -> anyhow::Result<Self> {
        let store = Self { path: dir.join("log-head-submission.json"), relayer, contract, chain_id };
        store.read()?;
        Ok(store)
    }

    /// Reads a bounded record and checks its deployment identity.
    /// Error cases:
    /// - a present file is unreadable, too large, or names another relayer, contract, or chain
    pub fn read(&self) -> anyhow::Result<Option<Reservation>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("opening log-head submission reservation"),
        };
        let mut bytes = Vec::new();
        file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes).context("reading log-head submission reservation")?;
        ensure!(bytes.len() as u64 <= RECORD_LIMIT, "log-head submission reservation is too large");
        let reservation: Reservation =
            serde_json::from_slice(&bytes).context("decoding log-head submission reservation")?;
        ensure!(reservation.version == 1, "unsupported log-head submission reservation version");
        ensure!(
            reservation.relayer == self.relayer
                && reservation.contract == self.contract
                && reservation.chain_id == self.chain_id,
            "log-head submission deployment mismatch"
        );
        ensure!(reservation.nonce < u64::MAX, "log-head relayer nonce is exhausted");
        Ok(Some(reservation))
    }

    /// The non-blocking form of the lock, for tests that assert it is free. Production
    /// operations use `wait_lock` inside their blocking worker.
    /// Error cases:
    /// - the lock file cannot be opened, or another poster owns it
    #[cfg(test)]
    pub fn lock(&self) -> anyhow::Result<File> {
        let file = self.open_lock()?;
        file.try_lock().context("log-head submission lock is already held")?;
        Ok(file)
    }

    /// The blocking form of `lock`, for the store's own operations on their blocking worker: the
    /// caller bounds its wait with a timeout, and this guard — owned by the worker — holds the
    /// lock until the operation finishes, even after the caller's timeout abandoned the wait.
    /// Error cases:
    /// - the lock file cannot be opened
    pub(super) fn wait_lock(&self) -> anyhow::Result<File> {
        let file = self.open_lock()?;
        file.lock().context("taking the log-head submission lock")?;
        Ok(file)
    }

    fn open_lock(&self) -> anyhow::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))
            .context("opening log-head submission lock")
    }

    /// Atomically replaces one bounded record. Lockless: `reserve` and `update` hold the lock
    /// across it; any other caller serializes against them some other way.
    /// Error cases:
    /// - serialization, write/sync, rename, or directory sync fails
    pub fn store(&self, reservation: &Reservation) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(reservation).context("encoding log-head submission reservation")?;
        ensure!(bytes.len() as u64 <= RECORD_LIMIT, "log-head submission reservation is too large");
        let staging = self.path.with_extension("json.tmp");
        let mut file = File::create(&staging).context("creating log-head submission staging file")?;
        file.write_all(&bytes).context("writing log-head submission reservation")?;
        file.sync_all().context("syncing log-head submission reservation")?;
        fs::rename(&staging, &self.path).context("replacing log-head submission reservation")?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .context("opening log-head submission for sync")?
            .sync_all()
            .context("syncing log-head submission file")?;
        let dir = self.path.parent().context("log-head submission has no directory")?;
        File::open(dir).context("opening log-head submission directory")?.sync_all().context("syncing log directory")
    }

    /// The durable reservation for the next send, adopting a consumed or different-headed one.
    /// A live reservation at the count is handed back pending, for the caller to bound in memory
    /// and reconcile against the chain. A previous nonce is advanced past only on
    /// chain evidence — the relayer's count passing it: while the count still names it, the nonce
    /// is reused, an unsent reservation rewritten for the new head, a pending one handed back for
    /// the caller to bound in memory, so a reservation the chain never consumed never leaves a gap
    /// the relayer blocks on.
    /// Error cases:
    /// - the lock cannot be taken, or the stored reservation cannot be read
    pub(super) fn reserve(&self, latest_nonce: u64, epoch: u64, head: Head, now: u64) -> anyhow::Result<Reserve> {
        let _guard = self.wait_lock()?;
        let previous = self.read()?;
        // A pending send dedups only at the live nonce: a stored nonce above the count is a
        // reorg or a planted reservation and reconciles down below, whatever head it names.
        if let Some(previous) = &previous
            && previous.nonce == latest_nonce
            && previous.head == Some((epoch, head))
            && previous.tx_hash.is_some()
        {
            return Ok(Reserve::Pending(previous.clone()));
        }
        let nonce = match &previous {
            // A stored nonce above the count — a chain rollback across a restart, or a planted
            // reservation — reconciles down: send at the count and fill the lowest outstanding
            // nonce rather than queue behind a gap the chain may never open.
            Some(previous) if previous.nonce > latest_nonce => latest_nonce,
            // The same head resends the identical transaction at the same nonce when its send
            // never happened; a pending send is the dedup above.
            Some(previous) if previous.head == Some((epoch, head)) && previous.nonce >= latest_nonce => previous.nonce,
            // Chain evidence consumed the outstanding nonce: adopt the fresh count.
            Some(previous) if latest_nonce > previous.nonce => latest_nonce,
            // The count still names the outstanding nonce: reuse it rather than skip it.
            Some(previous) if previous.tx_hash.is_none() => previous.nonce,
            // A send is pending at the outstanding nonce and the count still names it: the caller
            // bounds the wait in memory and reconciles the stored hash against the chain past it.
            Some(previous) => return Ok(Reserve::Pending(previous.clone())),
            None => latest_nonce,
        };
        let reservation = Reservation {
            version: 1,
            relayer: self.relayer,
            contract: self.contract,
            chain_id: self.chain_id,
            head: Some((epoch, head)),
            nonce,
            tx_hash: None,
            signature: None,
            calldata: None,
            parent_seq: 0,
            parent_link: B256::ZERO,
            stored_at: now,
        };
        self.store(&reservation)?;
        Ok(Reserve::Nonce(reservation))
    }

    /// Replaces a reservation, preserving its nonce and head. The lock is taken here, inside the
    /// blocking worker, and held through the write.
    /// Error cases:
    /// - the lock cannot be taken, or the write fails
    pub(super) fn update(&self, reservation: &Reservation) -> anyhow::Result<()> {
        let _guard = self.wait_lock()?;
        self.store(reservation)
    }
}

#[derive(Debug)]
pub(super) enum Reserve {
    /// A send is pending at the outstanding nonce and the chain count still names it: the caller
    /// — never this host-written file — bounds the wait, then reconciles the stored hash against
    /// the chain.
    Pending(Reservation),
    /// A fresh reservation to send at `reservation.nonce`.
    Nonce(Reservation),
}

impl Reserve {
    /// The reserved nonce; panics on the non-send variants.
    #[cfg(test)]
    pub(super) fn nonce(self) -> u64 {
        match self {
            Reserve::Nonce(reservation) => reservation.nonce,
            _ => panic!("expected a fresh nonce, found a pending decision"),
        }
    }
}
