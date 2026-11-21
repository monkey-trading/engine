//! A follower whose copy starts where a book no log holds ends (`Sequencer::take_over`).

use std::collections::BTreeSet;

use engine_core::types::AccountId;

use crate::daily_snapshots::Signed;
use crate::log::{CheckpointDomain, LogEntry, ReplayWorkers};
use crate::sequencer::replay::LiveReplay;
use crate::standby::Follower;
use crate::standby::replica::{Records, recording};

impl Follower {
    /// `open_pinned`, pinning nothing, with the replica built from `base`, a state no log holds, which
    /// the copy's first entry follows (`Sequencer::take_over`), or from `snapshot`. `base` gives a fresh
    /// engine for each build a retry starts.
    /// Error cases:
    /// - see `open_pinned`
    pub(crate) fn open_seeded(
        copy: (&std::path::Path, u64, ReplayWorkers),
        (domain, enclave, signers): (CheckpointDomain, AccountId, BTreeSet<AccountId>),
        records: Records,
        (base, snapshot): (impl Fn() -> engine_core::Engine, Option<Signed>),
        observe: impl FnMut(&LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let fresh = || recording(LiveReplay::seeded(domain, signers.clone(), base()), &records);
        let ids = (domain, enclave, signers.clone());
        Self::open_starting(copy, ids, fresh, &records, (None, &[]), snapshot, observe)
    }
}
