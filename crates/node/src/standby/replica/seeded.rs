//! A follower whose copy starts where a book no log holds ends (`Sequencer::take_over`).

use std::collections::BTreeSet;

use engine_core::types::AccountId;

use crate::log::{CheckpointDomain, LogEntry};
use crate::sequencer::replay::LiveReplay;
use crate::standby::Follower;
use crate::standby::replica::{Records, recording};

impl Follower {
    /// `open_pinned`, pinning nothing, with the replica built from `base`, a state no log holds, which
    /// the copy's first entry follows (`Sequencer::take_over`). `base` gives a fresh engine for each
    /// build a retry starts.
    /// Error cases:
    /// - see `open_pinned`
    pub(crate) fn open_seeded(
        dir: &std::path::Path,
        segment_commands: u64,
        (domain, enclave, signers): (CheckpointDomain, AccountId, BTreeSet<AccountId>),
        records: Records,
        base: impl Fn() -> engine_core::Engine,
        observe: impl FnMut(&LogEntry) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let fresh = || recording(LiveReplay::seeded(domain, signers.clone(), base()), &records);
        Self::open_with(dir, segment_commands, (domain, enclave, signers.clone()), fresh, None, observe)
    }
}
