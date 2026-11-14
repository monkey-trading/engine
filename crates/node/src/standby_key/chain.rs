//! What the standby key's rules read from the chain: the vault, its registry and the registry's
//! schedule, as `contracts/src/` declares them, every call at one block, the latest a strict majority
//! of the custody providers agrees on (`custody::StandbyReader`). That block is the sequencer's
//! confirmed one, never the finality tag, whose lag would let a promoted key's standby seal open
//! after its turn moved on.

use std::future::Future;

use alloy::primitives::{Address, B256, Bytes, keccak256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{Context, ensure};
use engine_core::types::AccountId;

/// The reason a promotion's assignment records, as `OperatorSchedule.PROMOTED` declares it.
const PROMOTED: u8 = 2;

sol! {
    function registry() external view returns (address);
    function schedule() external view returns (address);
    function checkpointSigner() external view returns (address);
    function signerEpoch() external view returns (uint64);
    // A struct return with dynamic members is offset-prefixed on the wire; a named struct return
    // decodes it, a flat tuple does not.
    struct CandidateReturn {
        address adminKey;
        address enclave;
        uint256 bond;
        bytes32 release;
        uint64 registration;
        string apiHostname;
        string handoverEndpoint;
    }
    function candidateOf(address candidate) external view returns (CandidateReturn memory);
    function releaseApproved(bytes32 approval) external view returns (bool);
    function standbyHolder(address key) external view returns (address);
    function promotable(address candidate) external view returns (address);
    struct TurnReturn {
        address candidate;
        uint16 position;
        uint8 reason;
        uint32 setId;
        uint64 generation;
        uint64 prior;
        bytes32 seed;
        address source;
    }
    function turnAt(uint64 timestamp) external view returns (uint64 turn, TurnReturn memory);
    struct AssignmentReturn {
        address candidate;
        address adminKey;
        uint32 setId;
        uint8 reason;
        address standbyKey;
        uint64 signerEpoch;
        uint64 headSeq;
        bytes32 headLink;
        uint64 checkpointSeq;
        bytes32 checkpointRoot;
        uint64 at;
    }
    struct RevisionReturn {
        bytes32 release;
        uint64 generation;
    }
    function assignmentOf(uint64 turn, uint16 position)
        external
        view
        returns (AssignmentReturn memory, RevisionReturn[] memory);
}

/// The chain, pinned at the latest block a strict majority of the custody providers agrees on.
pub(crate) trait StandbyChain: Sync {
    /// A pinned block, holding the read session that pinned it.
    type Block<'a>: AgreedBlock
    where
        Self: 'a;

    /// Pins the latest agreed block.
    /// Error cases:
    /// - the providers have no identity or block quorum, or serve another chain
    fn latest(&self) -> impl Future<Output = anyhow::Result<Self::Block<'_>>> + Send;
}

/// One agreed block, and the calls made at it.
pub(crate) trait AgreedBlock: Send + Sync {
    /// The block's time, in Unix seconds.
    fn time(&self) -> u64;

    /// The return data of a read-only call of `data` to `to` at the block, voted under `operation`.
    /// Error cases:
    /// - no quorum
    fn call(
        &self,
        operation: &'static str,
        to: Address,
        data: Bytes,
    ) -> impl Future<Output = anyhow::Result<Bytes>> + Send;
}

/// The contracts a standby key is attested and vouched for: the chain, the vault, its registry, and
/// the registry's schedule. The vault sets its registry once and the registry its schedule once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StandbyDomain {
    pub chain_id: u64,
    pub vault: Address,
    pub registry: Address,
    pub schedule: Address,
}

/// What a standby key is, by the schedule at the latest agreed block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StandbyState {
    /// The schedule lists it as the candidate's promotable standby key.
    Promotable,
    /// It is the vault's signer by a promotion that is still its turn's latest assignment, under the
    /// signer epoch the promotion started and the generation it reserved.
    Promoted(Promotion),
    /// Neither: unregistered, voided, replaced, or installed by a promotion its turn has moved past.
    Unusable(&'static str),
}

/// A promotion that installed a standby key, and the prefixes it fixed, which the promoted member's
/// copy must hold before it signs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Promotion {
    pub turn: u64,
    pub position: u16,
    pub signer_epoch: u64,
    /// The generation the promotion reserved, which the key is resealed under as the turn's keys.
    pub generation: u64,
    /// The outgoing epoch's latest head: its seq and link.
    pub head_seq: u64,
    pub head_link: B256,
    /// The vault's latest accepted checkpoint: its seq and root.
    pub checkpoint_seq: u64,
    pub checkpoint_root: B256,
}

/// What a vouch for a candidate's standby key depends on, at the latest agreed block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VouchView {
    /// The candidate's registered release, `None` while it is not registered.
    pub release: Option<B256>,
    /// Whether the timelock has approved that release for the candidate.
    pub approved: bool,
    /// The vault's current signer.
    pub signer: AccountId,
    /// Whether the schedule lists the vouching enclave's own standby key as promotable.
    pub own_promotable: bool,
}

impl StandbyDomain {
    /// The vault's registry and schedule at the latest agreed block, on `chain_id`.
    /// Error cases:
    /// - a read fails, or the vault names no registry or the registry no schedule
    pub(crate) async fn read(chain: &impl StandbyChain, chain_id: u64, vault: Address) -> anyhow::Result<Self> {
        let block = chain.latest().await?;
        let registry = call(&block, "standby_registry", vault, registryCall {}).await?;
        ensure!(!registry.is_zero(), "the vault names no operator registry");
        let schedule = call(&block, "standby_schedule", registry, scheduleCall {}).await?;
        ensure!(!schedule.is_zero(), "the registry names no schedule");
        Ok(Self { chain_id, vault, registry, schedule })
    }
}

/// What `key` is for `candidate` at the latest agreed block (see `StandbyState`). A promotion counts
/// while the turn in effect at the block's time names it as its latest assignment, that assignment
/// installed `key`, the vault still names `key` as its signer in the epoch the promotion started, and
/// the turn still holds the generation it reserved.
/// Error cases:
/// - a read fails, or the schedule records a promotion with no generation
pub(crate) async fn standby_state(
    chain: &impl StandbyChain,
    domain: &StandbyDomain,
    candidate: AccountId,
    key: AccountId,
) -> anyhow::Result<StandbyState> {
    let block = chain.latest().await?;
    let (candidate, key) = (Address::from(candidate.0), Address::from(key.0));
    if call(&block, "standby_promotable", domain.schedule, promotableCall { candidate }).await? == key {
        return Ok(StandbyState::Promotable);
    }
    let found = call(&block, "standby_turn", domain.schedule, turnAtCall { timestamp: block.time() }).await?;
    let (turn, named) = (found.turn, found._1);
    if turn == 0 || named.reason != PROMOTED || named.candidate != candidate {
        return Ok(StandbyState::Unusable("no promotion of the candidate is its turn's latest assignment"));
    }
    let position = named.position;
    let assigned = call(&block, "standby_assignment", domain.schedule, assignmentOfCall { turn, position }).await?;
    let (assignment, revisions) = (assigned._0, assigned._1);
    if assignment.standbyKey != key {
        return Ok(StandbyState::Unusable("the turn's latest promotion installed another key"));
    }
    let epoch = call(&block, "standby_signer_epoch", domain.vault, signerEpochCall {}).await?;
    if epoch != assignment.signerEpoch {
        return Ok(StandbyState::Unusable("the vault has left the signer epoch the promotion started"));
    }
    // The guardian's retirement clears the signer within the epoch.
    if call(&block, "standby_signer", domain.vault, checkpointSignerCall {}).await? != key {
        return Ok(StandbyState::Unusable("the vault no longer names the key as its signer"));
    }
    let generation = revisions.first().context("the schedule records a promotion with no generation")?.generation;
    if named.generation != generation {
        return Ok(StandbyState::Unusable("the member's release has moved since the promotion"));
    }
    Ok(StandbyState::Promoted(Promotion {
        turn,
        position,
        signer_epoch: epoch,
        generation,
        head_seq: assignment.headSeq,
        head_link: assignment.headLink,
        checkpoint_seq: assignment.checkpointSeq,
        checkpoint_root: assignment.checkpointRoot,
    }))
}

/// `candidate`'s registration, its release's approval, the vault's signer, and whether the schedule
/// lists `own`, the vouching enclave's standby key, as promotable, all at the latest agreed block.
/// Error cases:
/// - a read fails
pub(crate) async fn vouch_view(
    chain: &impl StandbyChain,
    domain: &StandbyDomain,
    candidate: AccountId,
    own: Option<AccountId>,
) -> anyhow::Result<VouchView> {
    let block = chain.latest().await?;
    let candidate = Address::from(candidate.0);
    let registered = call(&block, "standby_candidate", domain.registry, candidateOfCall { candidate }).await?;
    let release = (!registered.adminKey.is_zero()).then_some(registered.release);
    let approved = match release {
        Some(release) => {
            let approval = keccak256((candidate, release).abi_encode());
            call(&block, "standby_release_approved", domain.registry, releaseApprovedCall { approval }).await?
        }
        None => false,
    };
    let signer = call(&block, "standby_signer", domain.vault, checkpointSignerCall {}).await?;
    let own_promotable = match own {
        Some(own) => {
            let own = Address::from(own.0);
            let holder = call(&block, "standby_holder", domain.schedule, standbyHolderCall { key: own }).await?;
            !holder.is_zero()
                && call(&block, "standby_promotable", domain.schedule, promotableCall { candidate: holder }).await?
                    == own
        }
        None => false,
    };
    Ok(VouchView { release, approved, signer: AccountId(signer.0.0), own_promotable })
}

/// `request` made at `block` under `operation`, its return decoded.
/// Error cases:
/// - the call fails, or its return does not decode
async fn call<C: SolCall>(
    block: &impl AgreedBlock,
    operation: &'static str,
    to: Address,
    request: C,
) -> anyhow::Result<C::Return> {
    let out = block.call(operation, to, request.abi_encode().into()).await?;
    C::abi_decode_returns(&out).with_context(|| format!("decoding {operation}"))
}
