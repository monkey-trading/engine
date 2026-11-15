//! The turn in effect's current assignment at the latest block a strict majority of the custody
//! providers agrees on (#596). A skip moves a turn to the next member in its order at sequencer
//! confirmation, a finality period before a finalised read shows it, so the skipped-to member takes
//! the keys, and the key holder admits it, from this read. A handover binds the assignment its
//! successor read (`Binding`): the signer epoch, the turn, the assignment's position and its
//! generation; the holder admits only the member of the assignment it reads now, before the stop and
//! again before the keys go, and the successor reads it again before it seals them.

use std::sync::Arc;

use alloy::primitives::{Address, B256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::Context;
use engine_core::types::{AccountId, Millis};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::config::NodeConfig;
use crate::custody::StandbyReader;
use crate::handover::discovery::{Registered, members};
use crate::liveness::chain::rotationStateCall;
use crate::standby_key::chain::{
    AgreedBlock, StandbyChain, StandbyDomain, assignmentOfCall, signerEpochCall, turnAtCall,
};
use crate::tee::sealing::Release;

sol! {
    function latches(uint64 turn, uint16 position)
        external
        view
        returns (uint64 at, uint64 signerEpoch, address member, bytes32 release, uint64 generation);
    function headRelease(uint64 epoch) external view returns (bytes32);
}

/// Why the schedule names an assignment's member, as `OperatorSchedule` numbers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    Draw,
    /// The position before it did not take the turn within the take window.
    Skip,
    /// The liveness rule moved the vault's signer to the member's standby key: no handover.
    Promotion,
}

/// What a handover binds: the vault's signer epoch and the assignment of the turn its successor takes,
/// by its position and generation, as the successor read them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub epoch: u64,
    pub turn: u64,
    pub position: u16,
    pub generation: u64,
}

/// The turn in effect's current assignment at one agreed block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Assigned {
    pub binding: Binding,
    /// When the turn starts, in milliseconds.
    pub boundary: Millis,
    pub reason: Reason,
    /// The set the turn was drawn from.
    pub set: u32,
    pub candidate: AccountId,
    /// The admin key the set adopted for the member.
    pub admin_key: AccountId,
    /// The release the assignment names the member's holding in: its latest revision's.
    pub release: [u8; 32],
    /// Whether a head latched it: its member took the turn.
    pub latched: bool,
    /// The release of the signer epoch's newest v2 head, the enclave holding the keys; `None` before
    /// the epoch's first.
    pub head_release: Option<[u8; 32]>,
    /// The block's time, in seconds.
    pub at: u64,
}

/// Where the latest assignment is read.
pub(crate) trait Assignments: Send + Sync {
    /// The turn in effect's current assignment at the latest agreed block; `None` while the vault
    /// names no schedule, no turn is drawn by the block's time, or no turn names the operator.
    /// Error cases:
    /// - the providers have no quorum, or a call fails or does not decode
    fn assigned(&self) -> BoxFuture<'_, anyhow::Result<Option<Assigned>>>;

    /// The members of set `set` at the latest agreed block, each with its registration and whether it
    /// held an assignment of `turn` (`discovery::members`).
    /// Error cases:
    /// - the providers have no quorum, or a call fails or does not decode
    fn members(&self, set: u32, turn: u64) -> BoxFuture<'_, anyhow::Result<Vec<Registered>>>;
}

/// The latest assignment through a chain pinned at the latest agreed block, for the vault `vault`
/// on `chain_id`, with the LogHeads contract `log_heads`.
pub(crate) struct LatestAssignments<C> {
    pub chain: C,
    pub chain_id: u64,
    pub vault: Address,
    pub log_heads: Address,
}

impl<C: StandbyChain + Send> Assignments for LatestAssignments<C> {
    fn assigned(&self) -> BoxFuture<'_, anyhow::Result<Option<Assigned>>> {
        Box::pin(async move { read(&self.chain.latest().await?, self.chain_id, self.vault, self.log_heads).await })
    }

    fn members(&self, set: u32, turn: u64) -> BoxFuture<'_, anyhow::Result<Vec<Registered>>> {
        Box::pin(async move { members(&self.chain.latest().await?, self.chain_id, self.vault, (set, turn)).await })
    }
}

/// The assignment a successor binds its request to, and the reader it reads it again through before
/// it seals the keys; no binding without a reader, or while the latest assignment names another
/// release, when the finalised schedule alone decides.
pub(crate) struct Bound {
    pub binding: Option<Binding>,
    /// Whether the bound assignment is a skip, whose generation the finalised schedule shows only a
    /// finality period on.
    skip: bool,
    assignments: Option<Arc<dyn Assignments>>,
}

impl Bound {
    /// Binds nothing.
    pub(crate) fn none() -> Self {
        Self { binding: None, skip: false, assignments: None }
    }

    /// `assigned`, read again through `assignments`.
    pub(crate) fn of(assigned: &Assigned, assignments: Arc<dyn Assignments>) -> Self {
        Self { binding: Some(assigned.binding), skip: assigned.reason == Reason::Skip, assignments: Some(assignments) }
    }

    /// The bound skip's generation, which its member seals the keys in; `None` for no skip.
    pub(crate) fn skip_generation(&self) -> Option<u64> {
        self.binding.filter(|_| self.skip).map(|binding| binding.generation)
    }

    /// The latest assignment through `assignments`, when it names `release`.
    /// Error cases:
    /// - the read fails
    pub(crate) async fn read(assignments: Option<Arc<dyn Assignments>>, release: &Release) -> anyhow::Result<Self> {
        let Some(read) = assignments else { return Ok(Self::none()) };
        let assigned = read.assigned().await.context("reading the turn's latest assignment")?;
        Ok(match assigned.filter(|assigned| assigned.release == release.id()) {
            Some(assigned) => Self::of(&assigned, read),
            None => Self::none(),
        })
    }

    /// Reads the latest assignment again before the keys are sealed, after they arrived: a skip or a
    /// promotion since the request leaves this release without them, and the holder serves the member
    /// now named. Nothing to check with no binding.
    /// Error cases:
    /// - the read fails, or the latest assignment is not the one bound, naming `release`
    pub(crate) async fn recheck(&self, release: &Release) -> anyhow::Result<()> {
        let (Some(binding), Some(read)) = (self.binding, &self.assignments) else { return Ok(()) };
        let assigned = read.assigned().await.context("reading the turn's latest assignment before sealing")?;
        anyhow::ensure!(
            assigned.is_some_and(|assigned| assigned.binding == binding && assigned.release == release.id()),
            "the turn's assignment moved from {binding:?} before the keys were sealed: this release leaves them with \
             the holder, which serves the member now named"
        );
        Ok(())
    }
}

impl Assigned {
    /// Whether it names the holding of `release` in `generation`.
    pub(crate) fn names(&self, release: [u8; 32], generation: u64) -> bool {
        self.release == release && self.binding.generation == generation
    }
}

/// Whether the latest assignment `assignments` reads names the holding of `release` in `generation`;
/// false without a reader, while it names no turn, or when the read fails.
pub(crate) async fn names_holding(assignments: Option<&dyn Assignments>, release: [u8; 32], generation: u64) -> bool {
    let Some(read) = assignments else { return false };
    read.assigned().await.ok().flatten().is_some_and(|assigned| assigned.names(release, generation))
}

/// The latest assignment `cfg` reads, through a quorum of its own: with `[log_heads]`, whose
/// contract latches the takes and agrees the block, and EVM custody; `None` without them, when the
/// finalised schedule alone decides, as before #596.
/// Error cases:
/// - the custody section or the vault address is malformed
pub(crate) fn for_config(cfg: &NodeConfig, name: &'static str) -> anyhow::Result<Option<Arc<dyn Assignments>>> {
    let (Some(log_heads), Some(_)) = (cfg.log_heads.as_ref(), cfg.custody.evm.as_ref()) else { return Ok(None) };
    let chain = StandbyReader::connect_named(&cfg.custody, log_heads.contract, name)?;
    let vault = cfg.custody.vault.parse().context("custody.vault")?;
    Ok(Some(Arc::new(LatestAssignments {
        chain,
        chain_id: cfg.custody.chain_id,
        vault,
        log_heads: log_heads.contract,
    })))
}

/// The turn in effect's current assignment at `block`, as `Assignments::assigned` states it.
/// Error cases:
/// - a call fails or does not decode, or the turn has no boundary within the latest stamp
pub(crate) async fn read(
    block: &impl AgreedBlock,
    chain_id: u64,
    vault: Address,
    log_heads: Address,
) -> anyhow::Result<Option<Assigned>> {
    let Some(domain) = StandbyDomain::at_if_scheduled(block, chain_id, vault).await? else { return Ok(None) };
    let rotation = call(block, "assigned_rotation", domain.registry, rotationStateCall {}).await?;
    let found = call(block, "assigned_turn", domain.schedule, turnAtCall { timestamp: block.time() }).await?;
    if rotation.generation == 0 || found.turn == 0 {
        return Ok(None);
    }
    let (turn, named) = (found.turn, found._1);
    let epoch = call(block, "assigned_epoch", vault, signerEpochCall {}).await?;
    let position = named.position;
    let made = call(block, "assigned_assignment", domain.schedule, assignmentOfCall { turn, position }).await?;
    let latch = call(block, "assigned_latch", log_heads, latchesCall { turn, position }).await?;
    let head_release = call(block, "assigned_head_release", log_heads, headReleaseCall { epoch }).await?;
    let release = made._1.last().map(|revision| revision.release.0).unwrap_or_default();
    Ok(Some(Assigned {
        binding: Binding { epoch, turn, position, generation: named.generation },
        boundary: engine_core::turns::boundary(turn).context("the turn's boundary is out of range")?,
        reason: match named.reason {
            1 => Reason::Skip,
            2 => Reason::Promotion,
            _ => Reason::Draw,
        },
        set: named.setId,
        candidate: AccountId(named.candidate.0.0),
        admin_key: AccountId(made._0.adminKey.0.0),
        release,
        latched: latch.at != 0,
        head_release: (head_release != B256::ZERO).then_some(head_release.0),
        at: block.time(),
    }))
}

/// `request` made at `block` under `operation`, its return decoded.
/// Error cases:
/// - the call fails, or its return does not decode
pub(crate) async fn call<C: SolCall>(
    block: &impl AgreedBlock,
    operation: &'static str,
    to: Address,
    request: C,
) -> anyhow::Result<C::Return> {
    let out = block.call(operation, to, request.abi_encode().into()).await?;
    C::abi_decode_returns(&out).with_context(|| format!("decoding {operation}"))
}

#[cfg(test)]
pub(crate) mod tests;
