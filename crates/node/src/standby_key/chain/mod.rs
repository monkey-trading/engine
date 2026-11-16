//! What the standby key's rules read from the chain: the vault, its registry and the registry's
//! schedule, as `contracts/src/` declares them, every call at one block, the latest a strict majority
//! of the custody providers agrees on (`custody::StandbyReader`). That block is the sequencer's
//! confirmed one, never the finality tag, whose lag would let a promoted key's standby seal open
//! after its turn moved on.

use std::future::Future;

use alloy::primitives::{Address, B256, Bytes};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use engine_core::types::AccountId;

use crate::liveness::chain::rotationStateCall;

mod registration;
pub(crate) use registration::{registered_release, registration_at, release_approved};

/// The reason a promotion's assignment records, as `OperatorSchedule.PROMOTED` declares it.
const PROMOTED: u8 = 2;
/// How many signer epochs past finality a rebuilt copy reads keys for at most.
const UNFINAL_EPOCHS: u64 = 64;
/// How many turns drawn for a promoted member in the promotion's generation, one a turn, are followed
/// back to the promotion at most: past them the read fails, and the seal stays as it is.
const CARRIED_TURNS: usize = 64;

sol! {
    function registry() external view returns (address);
    function schedule() external view returns (address);
    function checkpointSigner() external view returns (address);
    function signerEpoch() external view returns (uint64);
    function signerAtEpoch(uint64 epoch) external view returns (address);
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
    function heldTurn(uint64 turn, address candidate) external view returns (bool);
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
    function turnOf(uint64 turn) external view returns (TurnReturn memory);
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
    function revisionOf(uint64 turn, uint16 position, uint256 index)
        external view returns (address candidate, RevisionReturn memory revision);
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

    /// Pins the highest block a strict majority holds final.
    /// Error cases:
    /// - as `latest`, or the chain has no final block yet
    fn finalized(&self) -> impl Future<Output = anyhow::Result<Self::Block<'_>>> + Send;
}

/// One agreed block, and the calls made at it.
pub(crate) trait AgreedBlock: Send + Sync {
    /// The block's time, in Unix seconds.
    fn time(&self) -> u64;

    /// The block's number.
    fn number(&self) -> u64;

    /// The block's hash.
    fn hash(&self) -> B256;

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
    /// The schedule has never taken it: a registration written or sent for it may still land.
    Unregistered,
    /// It is the vault's signer by a promotion that is still its turn's latest assignment, or carried
    /// into the same member's next drawn turn, under the signer epoch the promotion started and the
    /// generation it reserved.
    Promoted(Promotion),
    /// None of these: voided, replaced, or installed by a promotion its turn has moved past.
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
    /// The block time the promotion was made at, in seconds.
    pub at: u64,
    /// The number of the agreed block it was read at: no block below it says anything of it.
    pub block: u64,
    /// The turn in effect the promotion's generation is carried into at that block: `turn`, or the same
    /// member's drawn turn that continues it (`_carryNext`), which a start publishes the generation of.
    pub carried: u64,
}

impl Promotion {
    /// The generation the promotion reserved, by the turn it is the schedule's for and the turn in effect
    /// it is carried into, which a start publishes, so its first command in either is its `TurnTaken`.
    pub(crate) fn generations(&self) -> std::collections::BTreeMap<u64, u64> {
        [(self.turn, self.generation), (self.carried, self.generation)].into()
    }

    /// Whether `other` is this promotion: the same assignment, signer epoch, generation and
    /// commitments. The block time, the block read and the turn carried into are no part of it: a reorg
    /// that includes the same promotion again at a later block fixes the same history.
    pub(crate) fn same(&self, other: &Promotion) -> bool {
        Self { at: 0, block: 0, carried: 0, ..*self } == Self { at: 0, block: 0, carried: 0, ..*other }
    }
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
        Self::at(&chain.latest().await?, chain_id, vault).await
    }

    /// The vault's registry and schedule at `block`, on `chain_id`; `None` while the vault names no
    /// registry or the registry no schedule.
    /// Error cases:
    /// - a read fails
    pub(crate) async fn at_if_scheduled(
        block: &impl AgreedBlock,
        chain_id: u64,
        vault: Address,
    ) -> anyhow::Result<Option<Self>> {
        let registry = call(block, "standby_registry", vault, registryCall {}).await?;
        if registry.is_zero() {
            return Ok(None);
        }
        let schedule = call(block, "standby_schedule", registry, scheduleCall {}).await?;
        Ok((!schedule.is_zero()).then_some(Self { chain_id, vault, registry, schedule }))
    }

    /// The vault's registry and schedule at `block`, on `chain_id`.
    /// Error cases:
    /// - a read fails, or the vault names no registry or the registry no schedule
    pub(crate) async fn at(block: &impl AgreedBlock, chain_id: u64, vault: Address) -> anyhow::Result<Self> {
        let registry = call(block, "standby_registry", vault, registryCall {}).await?;
        ensure!(!registry.is_zero(), "the vault names no operator registry");
        let schedule = call(block, "standby_schedule", registry, scheduleCall {}).await?;
        ensure!(!schedule.is_zero(), "the registry names no schedule");
        Ok(Self { chain_id, vault, registry, schedule })
    }
}

/// What `key` is for `candidate` at the latest agreed block (see `StandbyState`). A promotion counts
/// while the turn in effect at the block's time names it as its latest assignment, or names that member
/// drawn in the promotion's generation, which the schedule carries into the member's next drawn turn
/// (`_carryNext`) and continues through each draw of it after (`_generation`), every turn back to the
/// promotion's drawn for it in that generation; that assignment installed `key`, the vault still names
/// `key` as its signer in the epoch the promotion started, and the turn in effect still holds the
/// generation the promotion reserved.
/// Error cases:
/// - a read fails, or the schedule records a promotion with no generation
/// - the turn in effect continues a promotion more than `CARRIED_TURNS` drawn turns back
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
    if call(&block, "standby_holder", domain.schedule, standbyHolderCall { key }).await?.is_zero() {
        return Ok(StandbyState::Unregistered);
    }
    let found = call(&block, "standby_turn", domain.schedule, turnAtCall { timestamp: block.time() }).await?;
    let (turn, named) = (found.turn, found._1);
    if turn == 0 || named.candidate != candidate {
        return Ok(StandbyState::Unusable("no promotion of the candidate is its turn's latest assignment"));
    }
    let (mut turn, mut latest, mut steps) = (turn, named.clone(), 0);
    while latest.reason != PROMOTED && latest.prior != 0 {
        let prior = call(&block, "standby_prior_turn", domain.schedule, turnOfCall { turn: latest.prior }).await?;
        if prior.candidate != candidate || prior.generation != named.generation {
            break;
        }
        ensure!(steps < CARRIED_TURNS, "turn {turn} continues a promotion more than {CARRIED_TURNS} drawn turns back");
        (turn, latest, steps) = (latest.prior, prior, steps + 1);
    }
    let Some(promotion) = promotion_of(&block, domain, AccountId(key.0.0), turn, latest.position).await? else {
        return Ok(StandbyState::Unusable("no promotion in the turn in effect, or carried into it, installed the key"));
    };
    if !in_effect(&block, domain, AccountId(key.0.0), &promotion).await? {
        return Ok(StandbyState::Unusable("the vault no longer names the key in the epoch the promotion started"));
    }
    if named.generation != promotion.generation {
        return Ok(StandbyState::Unusable("the turn in effect holds another generation than the promotion's"));
    }
    Ok(StandbyState::Promoted(Promotion { carried: found.turn, ..promotion }))
}

/// The promotion the schedule records for assignment `(turn, position)` at `block`, if one installed
/// `key`: the assignment is immutable once made, so a promotion that a reorg removed, or replaced with
/// another at the same position, reads as `None` or as other commitments.
/// Error cases:
/// - a read fails, or the schedule records a promotion with no generation
pub(crate) async fn promotion_of(
    block: &impl AgreedBlock,
    domain: &StandbyDomain,
    key: AccountId,
    turn: u64,
    position: u16,
) -> anyhow::Result<Option<Promotion>> {
    let assigned = call(block, "standby_assignment", domain.schedule, assignmentOfCall { turn, position }).await?;
    let (assignment, revisions) = (assigned._0, assigned._1);
    if assignment.reason != PROMOTED || assignment.standbyKey != Address::from(key.0) {
        return Ok(None);
    }
    let generation = revisions.first().context("the schedule records a promotion with no generation")?.generation;
    Ok(Some(Promotion {
        turn,
        position,
        signer_epoch: assignment.signerEpoch,
        generation,
        head_seq: assignment.headSeq,
        head_link: assignment.headLink,
        checkpoint_seq: assignment.checkpointSeq,
        checkpoint_root: assignment.checkpointRoot,
        at: assignment.at,
        block: block.number(),
        carried: turn,
    }))
}

/// Whether the vault at `block` still names `key` as its signer in the epoch `promotion` started: the
/// guardian's retirement clears the signer within the epoch, and a later signer change leaves it.
/// Error cases:
/// - a read fails
pub(crate) async fn in_effect(
    block: &impl AgreedBlock,
    domain: &StandbyDomain,
    key: AccountId,
    promotion: &Promotion,
) -> anyhow::Result<bool> {
    let epoch = call(block, "standby_signer_epoch", domain.vault, signerEpochCall {}).await?;
    let signer = call(block, "standby_signer", domain.vault, checkpointSignerCall {}).await?;
    Ok(epoch == promotion.signer_epoch && signer == Address::from(key.0))
}

/// Every key the vault at `block` names from signer epoch `from` on, up to `UNFINAL_EPOCHS` of them:
/// the keys promotions named before finality shows them.
/// Error cases:
/// - a read fails
pub(crate) async fn signers_from(
    block: &impl AgreedBlock,
    vault: Address,
    from: u64,
) -> anyhow::Result<Vec<AccountId>> {
    let latest = call(block, "standby_signer_epoch", vault, signerEpochCall {}).await?;
    let named = signers_in(block, vault, from.max(1)..=latest.min(from.saturating_add(UNFINAL_EPOCHS))).await?;
    Ok(named.into_iter().map(|(_, signer)| signer).collect())
}

/// The vault's signer epoch at `block`, and the keys it names in its latest `UNFINAL_EPOCHS` signer
/// epochs, each with its epoch, oldest first.
/// Error cases:
/// - a read fails
pub(crate) async fn recent_signers(
    block: &impl AgreedBlock,
    vault: Address,
) -> anyhow::Result<(u64, Vec<(u64, AccountId)>)> {
    let latest = call(block, "standby_signer_epoch", vault, signerEpochCall {}).await?;
    Ok((latest, signers_in(block, vault, latest.saturating_sub(UNFINAL_EPOCHS).max(1)..=latest).await?))
}

/// The keys the vault at `block` names in `epochs`, each with its epoch.
/// Error cases:
/// - a read fails
async fn signers_in(
    block: &impl AgreedBlock,
    vault: Address,
    epochs: std::ops::RangeInclusive<u64>,
) -> anyhow::Result<Vec<(u64, AccountId)>> {
    let mut signers = Vec::new();
    for epoch in epochs {
        let signer = call(block, "standby_signer_at_epoch", vault, signerAtEpochCall { epoch }).await?;
        if !signer.is_zero() {
            signers.push((epoch, AccountId(signer.0.0)));
        }
    }
    Ok(signers)
}

/// Whether the registry at `block` names `release` in `generation` as the operator in effect's: an
/// enclave opens keys sealed in a generation only while it does, as any start requires of the vault's
/// current release.
/// Error cases:
/// - a read fails
pub(crate) async fn names_release(
    block: &impl AgreedBlock,
    domain: &StandbyDomain,
    release: B256,
    generation: u64,
) -> anyhow::Result<bool> {
    let rotation = call(block, "standby_rotation", domain.registry, rotationStateCall {}).await?;
    Ok(rotation.release == release && rotation.generation == generation)
}

/// Whether the schedule at `block` takes a vouch from `voucher`: the vault's signer, or a promotable
/// standby key, as `registerStandby` checks it.
/// Error cases:
/// - a read fails
pub(crate) async fn may_vouch(
    block: &impl AgreedBlock,
    domain: &StandbyDomain,
    voucher: AccountId,
) -> anyhow::Result<bool> {
    let voucher = Address::from(voucher.0);
    if voucher.is_zero() {
        return Ok(false);
    }
    if call(block, "standby_signer", domain.vault, checkpointSignerCall {}).await? == voucher {
        return Ok(true);
    }
    let holder = call(block, "standby_holder", domain.schedule, standbyHolderCall { key: voucher }).await?;
    Ok(!holder.is_zero()
        && call(block, "standby_promotable", domain.schedule, promotableCall { candidate: holder }).await? == voucher)
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
    vouch_view_at(&chain.latest().await?, domain, candidate, own).await
}

/// The turn in effect at the latest agreed block when the schedule names `candidate` for it in
/// `generation`, which a promotion of `turn` in that generation is carried into; `turn` otherwise.
/// Error cases:
/// - a read fails
pub(crate) async fn carried_turn(
    chain: &impl StandbyChain,
    domain: &StandbyDomain,
    (candidate, generation, turn): (AccountId, u64, u64),
) -> anyhow::Result<u64> {
    let block = chain.latest().await?;
    let found = call(&block, "standby_turn", domain.schedule, turnAtCall { timestamp: block.time() }).await?;
    let named = found._1;
    let carried = found.turn != 0 && named.candidate == Address::from(candidate.0) && named.generation == generation;
    Ok(if carried { found.turn } else { turn })
}

/// `vouch_view`, at `block`.
/// Error cases:
/// - a read fails
pub(crate) async fn vouch_view_at(
    block: &impl AgreedBlock,
    domain: &StandbyDomain,
    candidate: AccountId,
    own: Option<AccountId>,
) -> anyhow::Result<VouchView> {
    let (release, approved) = registration_at(block, domain, candidate).await?;
    let signer = call(block, "standby_signer", domain.vault, checkpointSignerCall {}).await?;
    let own_promotable = match own {
        Some(own) => {
            let own = Address::from(own.0);
            let holder = call(block, "standby_holder", domain.schedule, standbyHolderCall { key: own }).await?;
            !holder.is_zero()
                && call(block, "standby_promotable", domain.schedule, promotableCall { candidate: holder }).await?
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
