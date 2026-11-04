//! The `RegistryLens` ABI (`contracts/src/RegistryLens.sol`) and the operator list a read of it
//! makes, checked as it is built: the answer `GET /v1/operators` serves.

use std::collections::BTreeSet;

use alloy::primitives::{Address, B256, U256};
use alloy::sol;
use anyhow::{Context, bail, ensure};
use engine_core::emissions::TokenUnits;
use engine_core::types::{AccountId, Millis};
use serde::Serialize;

sol! {
    struct Parameters {
        uint64 epochLength;
        uint64 minTerm;
        uint64 finalityMargin;
        uint16 marginBps;
        uint16 eligibilityBps;
        uint16 quorumBps;
        uint16 maxCandidates;
        uint256 minSelfBond;
        uint64 livenessTimeout;
    }
    struct Operator { address candidate; address adminKey; bytes32 release; uint64 since; }
    struct Election {
        uint64 epoch;
        address candidate;
        address adminKey;
        bytes32 release;
        uint64 effectiveAt;
        bool promoted;
    }
    struct Overview {
        bool rotationEnabled;
        Parameters parameters;
        uint64 openEpoch;
        uint64 openEpochStart;
        uint256 totalStaked;
        uint256 candidateCount;
        Operator inEffect;
        Operator named;
        Election pending;
        bool pendingPromotion;
    }
    struct Row {
        address candidate;
        address adminKey;
        address enclave;
        uint256 bond;
        bytes32 release;
        uint64 registration;
        string apiHostname;
        string handoverEndpoint;
        uint256 votes;
        bool eligible;
        uint8 role;
    }
    function overview() external view returns (Overview);
    function candidates(uint256 start, uint256 limit) external view returns (Row[]);
    function registry() external view returns (address);
}

/// The most listed candidates the registry allows (`OperatorRegistry.CANDIDATE_LIMIT`).
pub(crate) const CANDIDATE_LIMIT: usize = 100;

/// A block the list was read at: its height, hash and time, which the providers' quorum agreed on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct BlockStamp {
    pub number: u64,
    pub hash: B256,
    pub time_ms: Millis,
}

/// What `GET /v1/operators` answers: the registry's switch, parameters and operators, and every listed
/// candidate, all read at one block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperatorList {
    pub(crate) block: BlockStamp,
    registry: AccountId,
    lens: AccountId,
    rotation_enabled: bool,
    parameters: ParametersView,
    open_epoch: u64,
    open_epoch_start_ms: Millis,
    /// MONKEY base units staked across every account.
    total_staked: TokenUnits,
    /// The operator in effect at the block's time.
    operator_in_effect: OperatorView,
    /// The operator the vault names: a promoted standby from the moment of its promotion.
    named_operator: OperatorView,
    /// The election or promotion waiting to take effect.
    pending_election: Option<ElectionView>,
    pending_promotion: bool,
    /// Listed (approved) candidates by registration order, at most 100.
    candidates: Vec<CandidateView>,
}

/// The registry's election parameters in force; durations are seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ParametersView {
    epoch_length_secs: u64,
    min_term_secs: u64,
    finality_margin_secs: u64,
    margin_bps: u16,
    eligibility_bps: u16,
    quorum_bps: u16,
    max_candidates: u16,
    min_self_bond: TokenUnits,
    liveness_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct OperatorView {
    candidate: AccountId,
    admin_key: AccountId,
    release: B256,
    since_ms: Millis,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ElectionView {
    epoch: u64,
    candidate: AccountId,
    admin_key: AccountId,
    release: B256,
    effective_at_ms: Millis,
    /// Whether it is a liveness promotion, which names its release at once.
    promoted: bool,
}

/// A candidate's part in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    Candidate,
    /// Named by an election or promotion that has not yet taken effect.
    Elected,
    /// The operator in effect.
    Operator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CandidateView {
    address: AccountId,
    admin_key: AccountId,
    enclave: AccountId,
    self_bond: TokenUnits,
    release: B256,
    /// How many registrations preceded this one: lower is earlier.
    registration: u64,
    api_hostname: String,
    handover_endpoint: String,
    votes: TokenUnits,
    eligible: bool,
    role: Role,
}

/// The lens's answers at one block, before they are checked and assembled.
pub(super) struct Reads {
    pub block: BlockStamp,
    pub registry: Address,
    pub lens: Address,
    pub overview: Overview,
    pub rows: Vec<Row>,
}

/// A MONKEY amount, refusing one past `u128`.
/// Error cases:
/// - the amount exceeds `u128`
fn units(amount: U256, what: &str) -> anyhow::Result<TokenUnits> {
    crate::custody::staking::monkey_units(amount)
        .map(TokenUnits)
        .with_context(|| format!("{what} {amount} exceeds u128"))
}

/// A chain time in seconds as milliseconds.
/// Error cases:
/// - the milliseconds pass `i64`
fn millis(seconds: u64, what: &str) -> anyhow::Result<Millis> {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000))
        .with_context(|| format!("{what} {seconds} is out of range"))
}

impl OperatorView {
    fn new(operator: &Operator, what: &str) -> anyhow::Result<Self> {
        Ok(Self {
            candidate: AccountId(operator.candidate.into()),
            admin_key: AccountId(operator.adminKey.into()),
            release: operator.release,
            since_ms: millis(operator.since, what)?,
        })
    }
}

impl OperatorList {
    /// How many candidates the list holds.
    pub(super) fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    /// The list `reads` make, checked against each other: the lens names the configured registry,
    /// the overview's count is the rows read and within the registry's limit, no candidate is listed
    /// twice or as zero, and every role is one the lens defines.
    /// Error cases:
    /// - any of those checks fails, or an amount or time is out of range
    pub(super) fn assemble(reads: Reads, configured_registry: Address) -> anyhow::Result<Self> {
        let Reads { block, registry, lens, overview, mut rows } = reads;
        ensure!(
            registry == configured_registry,
            "the lens reads registry {registry}, not the configured {configured_registry}"
        );
        let count = usize::try_from(overview.candidateCount).ok().filter(|count| *count <= CANDIDATE_LIMIT);
        let count = count.with_context(|| format!("the lens lists {} candidates", overview.candidateCount))?;
        ensure!(rows.len() == count, "the lens listed {count} candidates and returned {} rows", rows.len());
        let mut seen = BTreeSet::new();
        for row in &rows {
            ensure!(row.candidate != Address::ZERO, "the lens returned a zero candidate");
            ensure!(seen.insert(row.candidate), "the lens returned candidate {} twice", row.candidate);
        }
        rows.sort_by_key(|row| row.registration);
        let candidates = rows.iter().map(candidate).collect::<anyhow::Result<Vec<_>>>()?;
        let pending = &overview.pending;
        let p = &overview.parameters;
        Ok(Self {
            block,
            registry: AccountId(registry.into()),
            lens: AccountId(lens.into()),
            rotation_enabled: overview.rotationEnabled,
            parameters: ParametersView {
                epoch_length_secs: p.epochLength,
                min_term_secs: p.minTerm,
                finality_margin_secs: p.finalityMargin,
                margin_bps: p.marginBps,
                eligibility_bps: p.eligibilityBps,
                quorum_bps: p.quorumBps,
                max_candidates: p.maxCandidates,
                min_self_bond: units(p.minSelfBond, "the minimum self-bond")?,
                liveness_timeout_secs: p.livenessTimeout,
            },
            open_epoch: overview.openEpoch,
            open_epoch_start_ms: millis(overview.openEpochStart, "the open epoch's start")?,
            total_staked: units(overview.totalStaked, "the total staked")?,
            operator_in_effect: OperatorView::new(&overview.inEffect, "the operator in effect's start")?,
            named_operator: OperatorView::new(&overview.named, "the named operator's start")?,
            pending_election: (pending.effectiveAt != 0)
                .then(|| {
                    Ok::<_, anyhow::Error>(ElectionView {
                        epoch: pending.epoch,
                        candidate: AccountId(pending.candidate.into()),
                        admin_key: AccountId(pending.adminKey.into()),
                        release: pending.release,
                        effective_at_ms: millis(pending.effectiveAt, "the pending election's effective time")?,
                        promoted: pending.promoted,
                    })
                })
                .transpose()?,
            pending_promotion: overview.pendingPromotion,
            candidates,
        })
    }
}

/// One candidate's view.
/// Error cases:
/// - an amount exceeds `u128`, or the role is none the lens defines
fn candidate(row: &Row) -> anyhow::Result<CandidateView> {
    let role = match row.role {
        0 => Role::Candidate,
        1 => Role::Elected,
        2 => Role::Operator,
        other => bail!("the lens returned role {other} for candidate {}", row.candidate),
    };
    Ok(CandidateView {
        address: AccountId(row.candidate.into()),
        admin_key: AccountId(row.adminKey.into()),
        enclave: AccountId(row.enclave.into()),
        self_bond: units(row.bond, "a self-bond")?,
        release: row.release,
        registration: row.registration,
        api_hostname: row.apiHostname.clone(),
        handover_endpoint: row.handoverEndpoint.clone(),
        votes: units(row.votes, "a candidate's votes")?,
        eligible: row.eligible,
        role,
    })
}
