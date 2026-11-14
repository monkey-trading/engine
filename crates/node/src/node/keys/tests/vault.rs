//! The vault a start reads, as a test sets it: its current release and fold, the schedule's turn,
//! and an election's incumbent.

use std::sync::{Arc, Mutex};

use anyhow::{Context, bail, ensure};
use engine_core::Fixed;
use engine_core::types::AccountId;

use crate::checkpoints::{CheckpointChain, CurrentRelease, Observation, VaultState, View};
use crate::log::Checkpoint;

/// A vault whose current release is the one `current` holds; with `synced` set it reads the fold
/// too, and that is the release its generation has folded.
pub(super) struct Vault {
    pub(super) current: Arc<Mutex<CurrentRelease>>,
    pub(super) synced: Arc<Mutex<Option<[u8; 32]>>>,
    /// The turn the finalised schedule names, at any time at or after its boundary.
    pub(super) scheduled: Arc<Mutex<Option<crate::checkpoints::ScheduledTurn>>>,
    /// The incumbent an election in effect replaced, with rotation on; none with it off.
    pub(super) elected_from: Arc<Mutex<Option<crate::checkpoints::HandoverSource>>>,
}

impl Vault {
    pub(super) fn naming(current: CurrentRelease) -> Self {
        Self {
            current: Arc::new(Mutex::new(current)),
            synced: Arc::new(Mutex::new(None)),
            scheduled: Arc::default(),
            elected_from: Arc::default(),
        }
    }
}

impl CheckpointChain for Vault {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        let zero = Fixed::ZERO;
        Ok(VaultState {
            height: 1,
            signer: AccountId([0; 20]),
            checkpoint: None,
            balance: zero,
            deposits: 0,
            deposited: zero,
            distributions_paid: zero,
        })
    }

    async fn post(&self, _checkpoint: &Checkpoint) -> anyhow::Result<()> {
        bail!("this vault takes no checkpoints")
    }

    async fn current_release(&self, at: &Observation) -> anyhow::Result<CurrentRelease> {
        ensure!(at.view == View::Finalized, "the current release is read at finality");
        Ok(*self.current.lock().unwrap())
    }

    async fn scheduled_turn(
        &self,
        _at: &Observation,
        now_ms: engine_core::types::Millis,
    ) -> anyhow::Result<Option<crate::checkpoints::ScheduledTurn>> {
        Ok(self.scheduled.lock().unwrap().clone().filter(|turn| turn.boundary <= now_ms))
    }

    fn reads_release_fold(&self) -> bool {
        self.synced.lock().unwrap().is_some()
    }

    async fn rotation_enabled(&self) -> anyhow::Result<bool> {
        Ok(self.elected_from.lock().unwrap().is_some())
    }

    async fn handover_source(&self) -> anyhow::Result<crate::checkpoints::HandoverSource> {
        self.elected_from.lock().unwrap().clone().context("rotation is off")
    }

    async fn synced_release(&self, at: &Observation) -> anyhow::Result<[u8; 32]> {
        ensure!(at.view == View::Finalized, "the folded release is read at finality");
        let synced = *self.synced.lock().unwrap();
        synced.context("this vault reads no fold")
    }
}
