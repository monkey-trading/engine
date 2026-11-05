//! The test vault, whose current release and registry switch the test sets.

use std::sync::atomic::AtomicBool;

use anyhow::Context as _;
use engine_core::types::Millis;

use crate::checkpoints::ScheduledTurn;

use super::*;

/// A vault that names `signer`, and the release `current` holds as its current one; with `synced`
/// set it reads the fold too, and that is the release its generation has folded.
pub(super) struct Vault {
    pub(super) signer: AccountId,
    pub(super) current: Arc<Mutex<CurrentRelease>>,
    /// The release the vault's generation has folded; `None` reads no fold.
    pub(super) synced: Arc<Mutex<Option<[u8; 32]>>>,
    /// The registry's rotation switch.
    pub(super) rotation: Rotation,
    /// The turn the finalised schedule names, at any time at or after its boundary.
    pub(super) scheduled: Arc<Mutex<Option<ScheduledTurn>>>,
}

/// The registry's rotation switch as the vault reads it, at the latest block and at finality, and
/// whether reading it fails.
#[derive(Clone, Default)]
pub(super) struct Rotation {
    pub(super) latest: Arc<AtomicBool>,
    pub(super) finalized: Arc<AtomicBool>,
    pub(super) fails: Arc<AtomicBool>,
}

impl CheckpointChain for Vault {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        let zero = Fixed::ZERO;
        Ok(VaultState {
            height: 1,
            signer: self.signer,
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

    async fn rotation_enabled(&self) -> anyhow::Result<bool> {
        ensure!(!self.rotation.fails.load(Ordering::SeqCst), "the rotation switch cannot be read");
        Ok(self.rotation.latest.load(Ordering::SeqCst))
    }

    async fn rotation_enabled_at(&self, at: &Observation) -> anyhow::Result<bool> {
        ensure!(at.view == View::Finalized, "the rotation switch is read at finality");
        ensure!(!self.rotation.fails.load(Ordering::SeqCst), "the rotation switch cannot be read");
        Ok(self.rotation.finalized.load(Ordering::SeqCst))
    }

    async fn scheduled_turn(&self, at: &Observation, now_ms: Millis) -> anyhow::Result<Option<ScheduledTurn>> {
        ensure!(at.view == View::Finalized, "the schedule is read at finality");
        Ok(self.scheduled.lock().unwrap().clone().filter(|turn| turn.boundary <= now_ms))
    }

    fn reads_release_fold(&self) -> bool {
        self.synced.lock().unwrap().is_some()
    }

    async fn synced_release(&self, at: &Observation) -> anyhow::Result<[u8; 32]> {
        ensure!(at.view == View::Finalized, "the folded release is read at finality");
        let synced = *self.synced.lock().unwrap();
        synced.context("this vault reads no fold")
    }
}
