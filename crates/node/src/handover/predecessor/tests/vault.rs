//! The test vault, whose current release and registry switch the test sets.

use std::sync::atomic::AtomicBool;

use anyhow::Context as _;
use engine_core::types::Millis;

use crate::checkpoints::ScheduledTurn;

use super::*;

/// A vault that names `signer`, and the release `current` holds as its current one; with `synced`
/// set it reads the fold too, and that is the release its generation has folded. Its earlier epoch's
/// signer is the test sequencer's key, which signs the logs the tests' nodes write. The checkpoint it
/// accepted last is the one `finalized` holds, at every view.
pub(super) struct Vault {
    pub(super) signer: AccountId,
    pub(super) current: Arc<Mutex<CurrentRelease>>,
    /// The release the vault's generation has folded; `None` reads no fold.
    pub(super) synced: Arc<Mutex<Option<[u8; 32]>>>,
    /// The registry's rotation switch.
    pub(super) rotation: Rotation,
    /// The turn the finalised schedule names, at any time at or after its boundary.
    pub(super) scheduled: Arc<Mutex<Option<ScheduledTurn>>>,
    /// The checkpoint it accepted last, `(seq, root)`; none until a test sets one.
    pub(super) finalized: Arc<Mutex<Option<(u64, String)>>>,
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
            checkpoint: self.finalized.lock().unwrap().clone(),
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

    /// The checkpoint `finalized` holds, signed in the earlier epoch by the test sequencer's key, which signed it; any
    /// other seq is one it accepted none at.
    async fn accepted_checkpoints(
        &self,
        at: &Observation,
        sequences: &[u64],
    ) -> anyhow::Result<Vec<crate::checkpoints::AcceptedCheckpoint>> {
        ensure!(at.view == View::Finalized, "checkpoint history is read at finality");
        let finalized = self.finalized.lock().unwrap().clone();
        let signer = AccountId(crate::tee::EnclaveKey::test_key().address());
        let accepted = |seq: &u64| {
            let (_, root) = finalized.clone().filter(|(at, _)| at == seq).context("checkpoint was not accepted")?;
            anyhow::Ok(crate::checkpoints::AcceptedCheckpoint { seq: *seq, root, signer, signer_epoch: 1 })
        };
        sequences.iter().map(accepted).collect()
    }

    async fn epoch_signers(&self, at: &Observation, _epochs: &[u64]) -> anyhow::Result<Vec<AccountId>> {
        ensure!(at.view == View::Finalized, "signer history is read at finality");
        Ok(vec![AccountId(crate::tee::EnclaveKey::test_key().address())])
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

/// The custody quorum's reads at the endpoint's block: what the test vault reads there by default, as
/// when the endpoint tells the truth; once a test sets `reads`, what it sets, as when the endpoint lies.
pub(super) struct Majority {
    current: Arc<Mutex<CurrentRelease>>,
    synced: Arc<Mutex<Option<[u8; 32]>>>,
    scheduled: Arc<Mutex<Option<ScheduledTurn>>>,
    rotation: Rotation,
    pub(super) reads: Mutex<Reads>,
}

/// What the quorum reads in place of what the test vault reads, where set.
#[derive(Default)]
pub(super) struct Reads {
    pub(super) current: Option<CurrentRelease>,
    pub(super) synced: Option<[u8; 32]>,
    pub(super) turn: Option<Option<ScheduledTurn>>,
    /// At the endpoint's block and at the quorum's head.
    pub(super) rotation: Option<(bool, bool)>,
}

impl Majority {
    /// A quorum reading what `vault` reads.
    pub(super) fn of(vault: &Vault) -> Self {
        Self {
            current: vault.current.clone(),
            synced: vault.synced.clone(),
            scheduled: vault.scheduled.clone(),
            rotation: vault.rotation.clone(),
            reads: Mutex::default(),
        }
    }
}

impl crate::handover::quorum::ReleaseQuorum for Majority {
    fn held<'a>(
        &'a self,
        _at: &'a Observation,
        now_ms: Option<Millis>,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<crate::handover::quorum::Held>> {
        let reads = self.reads.lock().unwrap();
        let current = reads.current.unwrap_or(*self.current.lock().unwrap());
        let synced = reads.synced.or(*self.synced.lock().unwrap()).unwrap_or(current.id);
        let turn = reads.turn.clone().unwrap_or_else(|| self.scheduled.lock().unwrap().clone());
        let turn = turn.filter(|_| now_ms.is_some()).as_ref().map(crate::handover::quorum::TurnFacts::from);
        Box::pin(async move { Ok(crate::handover::quorum::Held { current, synced, turn }) })
    }

    fn rotation<'a>(
        &'a self,
        _at: &'a Observation,
    ) -> futures_util::future::BoxFuture<'a, anyhow::Result<(bool, bool)>> {
        let mirrored = (self.rotation.finalized.load(Ordering::SeqCst), self.rotation.latest.load(Ordering::SeqCst));
        let rotation = self.reads.lock().unwrap().rotation.unwrap_or(mirrored);
        Box::pin(async move { Ok(rotation) })
    }
}
