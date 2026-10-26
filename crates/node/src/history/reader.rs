//! A page pins one complete history version through the end of its actual blocking read.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use rusqlite::ErrorCode;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::spawn_blocking;

use crate::authenticated::{Lease, Store};
use crate::history::authenticated::{Head, Index};
use crate::history::{Page, PageQuery, store};
use crate::snapshot::head::StoreId;

/// Matches the legacy reader's per-node blocking-IO admission bound.
const MAX_READS: usize = 16;

/// Either the legacy SQL index or the authenticated actor's latest complete immutable version.
#[derive(Clone)]
pub struct Reader {
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    Legacy(store::Reader),
    Authenticated(AuthenticatedReader),
}

#[derive(Clone)]
struct AuthenticatedReader {
    ready: watch::Receiver<bool>,
    published: watch::Receiver<Option<Arc<Published>>>,
    repair: mpsc::Sender<StoreId>,
    permits: Arc<Semaphore>,
}

/// A failed proof invalidates the generation, including heads published after that reader began.
pub(super) struct Generation {
    pub store: StoreId,
    valid: AtomicBool,
}

/// The actor publishes this only after a complete durable update or completed verified repair.
pub(super) struct Published {
    pub store: Store,
    pub head: Head,
    pub generation: Arc<Generation>,
    _lease: Lease,
}

impl Generation {
    /// Creates trusted liveness state; it is never restored from host metadata.
    pub(super) fn new(store: StoreId) -> Arc<Self> {
        Arc::new(Self { store, valid: AtomicBool::new(true) })
    }

    /// False until the owner replaces a detected-damaged generation with verified rebuilt data.
    pub(super) fn valid(&self) -> bool {
        self.valid.load(Ordering::Acquire)
    }

    /// Requests repair without changing protocol state, roots, or any persisted input.
    pub(super) fn invalidate(&self) {
        self.valid.store(false, Ordering::Release);
    }
}

impl Published {
    /// Checks fixed metadata and keeps the exact version alive for every cloned reader snapshot.
    /// Error cases:
    /// - metadata verification, root leasing, or generation validity fails
    pub(super) fn new(store: Store, head: Head, generation: Arc<Generation>) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(generation.valid(), "history generation requires repair");
        let lease = store.lease(&head.root)?;
        Index::resume(store.clone(), head.clone())?;
        Ok(Arc::new(Self { store, head, generation, _lease: lease }))
    }
}

impl Reader {
    /// Preserves the legacy reader's public constructor and eager legacy-integrity contract.
    pub fn new(path: PathBuf, ready: watch::Receiver<bool>) -> Self {
        Self { backend: Backend::Legacy(store::Reader::new(path, ready)) }
    }

    /// Connects the single storage owner's complete-head publication and bounded repair queue.
    pub(super) fn authenticated(
        ready: watch::Receiver<bool>,
        published: watch::Receiver<Option<Arc<Published>>>,
        repair: mpsc::Sender<StoreId>,
    ) -> Self {
        Self {
            backend: Backend::Authenticated(AuthenticatedReader {
                ready,
                published,
                repair,
                permits: Arc::new(Semaphore::new(MAX_READS)),
            }),
        }
    }

    /// Returns an exact page, or CatchingUp while startup/repair has not reached its required tip.
    /// Error cases:
    /// - storage, proof, page validation, or the blocking worker fails
    pub async fn page(&self, query: PageQuery) -> anyhow::Result<Option<Page>> {
        match &self.backend {
            Backend::Legacy(reader) => reader.page(query).await,
            Backend::Authenticated(reader) => reader.page(query).await,
        }
    }
}

impl AuthenticatedReader {
    /// Moves both the admission permit and immutable-version lease into the blocking closure.
    /// Error cases:
    /// - required data cannot be proved or decoded, or the blocking worker panics
    async fn page(&self, query: PageQuery) -> anyhow::Result<Option<Page>> {
        if !*self.ready.borrow() {
            return Ok(None);
        }
        query.validate()?;
        let Some(published) = self.published.borrow().clone() else {
            return Ok(None);
        };
        if !published.generation.valid() {
            return Ok(None);
        }
        let permit = self.permits.clone().acquire_owned().await.expect("the history read semaphore is never closed");
        if !published.generation.valid() {
            return Ok(None);
        }
        let repair = self.repair.clone();
        spawn_blocking(move || {
            let _permit = permit;
            let page =
                Index::resume(published.store.clone(), published.head.clone()).and_then(|index| index.page(query));
            if let Err(error) = &page
                && !transient(error)
            {
                published.generation.invalidate();
                // A full queue already wakes the owner. It always checks current validity, so
                // an older generation's queued signal cannot lose a newer repair request.
                let _ = repair.try_send(published.generation.store);
            }
            page.map(Some)
        })
        .await
        .context("authenticated history page worker")?
    }
}

/// Lock contention, or a file the process cannot open or memory it cannot get, in SQLite or for a
/// returned value, does not establish
/// corrupt data, and a rebuild would not fix it; none triggers a full verified rebuild.
pub(crate) fn transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::collections::TryReserveError>().is_some()
            || matches!(cause.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(error, _))
            if matches!(
                error.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked | ErrorCode::CannotOpen | ErrorCode::OutOfMemory
            ))
    })
}

#[cfg(test)]
mod tests;
