//! Mirroring the staking contract's finalised events, and from `V37` the operator registry's, into
//! the engine. The deposit watcher calls `mirror` once per confirmed poll, and only through the
//! Ethereum-finalised boundary the poll read: a staking mirror has no reversal, so a merely confirmed
//! event must never become a permanent engine balance or election. Events hold until `InitStaking` has
//! applied, credit once under `tx:logIndex`, and advance the engine's authenticated mirrored-through
//! frontier, so a host cursor cannot skip an event and a restart that re-reads a range mirrors nothing
//! twice.

use std::collections::BTreeMap;
use std::path::Path;

use alloy::primitives::{B256, U256};
use anyhow::{Context, anyhow, bail};
use engine_core::command::{AdminCommand, Command};
use engine_core::election::{RegistryElection, RegistryInstallation};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::governance::PassedProposal;
use engine_core::operator_faults::ProvenFault;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{RegistryRotationSet, RegistryTurn, SetMember, TurnReason};
use engine_core::types::{AccountId, Millis};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::Instrument;
use tracing::{debug, info, info_span};

use crate::custody::chain::{BlockLink, LinkedRun};
use crate::custody::credit::{query, submit, sync};
use crate::custody::evm::{BlockRef, VaultReader};
use crate::custody::sizing::AdaptiveSize;
use crate::custody::stall::{PollError, Stall};
use crate::sequencer::Request;
/// One mirrorable event: from the staking contract, a stake or an unstake the cooldown has passed;
/// from the operator registry, a rotation switch or an election; from its schedule, a set or a turn;
/// from the vote contract, a passed proposal; from the fault proof contract, a proven fault.
#[derive(Clone, Debug, PartialEq)]
pub struct StakingEvent {
    pub block: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub log_index: u64,
    pub kind: StakingEventKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StakingEventKind {
    /// `amount` MONKEY base units staked by `account`.
    Staked { account: AccountId, amount: u128 },
    /// `amount` MONKEY base units returned to `account` after the cooldown.
    UnstakeCompleted { account: AccountId, amount: u128 },
    /// The registry turned rotation on or off, cancelling an election not yet in effect or not.
    RotationChanged { enabled: bool, election_cancelled: bool },
    /// The registry elected `candidate`, whose `admin_key` takes effect at `effective_at`
    /// milliseconds.
    Elected { epoch: u64, candidate: AccountId, admin_key: AccountId, effective_at: Millis },
    /// The timelock installed `candidate`, whose `admin_key` takes effect seven days after the
    /// mirror is logged.
    Installed { candidate: AccountId, admin_key: AccountId },
    /// The schedule adopted rotation set `set` at `epoch`'s close, taking effect at `effective_at`
    /// milliseconds.
    RotationSetAdopted { set: u32, epoch: u64, effective_at: Millis, quorum: bool, members: Vec<SetMember> },
    /// The schedule named `candidate`, under `admin_key`, for `turn`.
    TurnAssigned { turn: u64, set: u32, position: u16, reason: TurnReason, candidate: AccountId, admin_key: AccountId },
    /// The vote contract passed `proposal`, whose command bytes hash to `command_hash` and are the
    /// canonical JSON of `command`, or of no admin command when it is `None`.
    ProposalPassed { proposal: u64, fast: bool, command_hash: B256, command: Option<Box<AdminCommand>> },
    /// The fault proof contract proved that `member`'s enclave signed two heads of `turn`'s assignment
    /// at `position` under `signer_epoch` that disagree at `seq`.
    FaultProven { member: AccountId, turn: u64, position: u16, signer_epoch: u64, seq: u64 },
}

impl StakingEvent {
    /// The engine's reference for this event: `<tx hash>:<log index>`, like a deposit's.
    pub fn chain_ref(&self) -> String {
        format!("{}:{}", self.tx_hash, self.log_index)
    }

    /// The mirror command this event carries, naming the finalised block it was read from so the
    /// engine advances the authenticated mirrored-through frontier.
    fn command(&self) -> Command {
        let (block, chain_ref) = (self.block, self.chain_ref());
        match self.kind.clone() {
            StakingEventKind::Staked { account, amount } => {
                Command::Stake { account, amount: TokenUnits(amount), block, chain_ref }
            }
            StakingEventKind::UnstakeCompleted { account, amount } => {
                Command::Unstake { account, amount: TokenUnits(amount), block, chain_ref }
            }
            StakingEventKind::RotationChanged { enabled, election_cancelled } => {
                Command::SwitchRotation { enabled, election_cancelled, block, chain_ref }
            }
            StakingEventKind::Elected { epoch, candidate, admin_key, effective_at } => {
                Command::ElectOperator(RegistryElection { epoch, candidate, admin_key, effective_at, block, chain_ref })
            }
            StakingEventKind::Installed { candidate, admin_key } => {
                Command::InstallOperator(RegistryInstallation { candidate, admin_key, block, chain_ref })
            }
            StakingEventKind::RotationSetAdopted { set, epoch, effective_at, quorum, members } => {
                Command::AdoptRotationSet(RegistryRotationSet {
                    set,
                    epoch,
                    effective_at,
                    quorum,
                    members,
                    block,
                    chain_ref,
                })
            }
            StakingEventKind::TurnAssigned { turn, set, position, reason, candidate, admin_key } => {
                Command::AssignTurn(RegistryTurn {
                    turn,
                    set,
                    position,
                    reason,
                    candidate,
                    admin_key,
                    block,
                    chain_ref,
                })
            }
            StakingEventKind::ProposalPassed { proposal, fast, command_hash, command } => {
                let command = command.map(|command| *command);
                Command::PassProposal(Box::new(PassedProposal {
                    proposal,
                    fast,
                    command_hash,
                    command,
                    block,
                    chain_ref,
                }))
            }
            StakingEventKind::FaultProven { member, turn, position, signer_epoch, seq } => {
                Command::ProveFault(ProvenFault { member, turn, position, signer_epoch, seq, block, chain_ref })
            }
        }
    }
}

/// A host-stored cache of the staking scan position. The watcher derives its authoritative
/// start from the engine's completed frontier, or the configured start before any completion;
/// this file cannot advance it. `log_seq` refuses a cursor ahead of the recovered log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StakingCursor {
    pub chain_id: u64,
    pub contract: alloy::primitives::Address,
    /// The first block not yet mirrored.
    pub next_block: u64,
    /// The log's last seq when the cursor last moved past an event; the recovered log must reach it.
    pub log_seq: Option<u64>,
}

impl StakingCursor {
    /// The cursor stored at `path`, or one at `start_block` when there is none.
    /// Error cases:
    /// - the file exists but cannot be read or parsed
    /// - it belongs to another chain or contract
    /// - it has mirrored events the log does not hold
    pub fn load(
        path: &Path,
        chain_id: u64,
        contract: alloy::primitives::Address,
        start_block: u64,
        log_seq: Option<u64>,
    ) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StakingCursor { chain_id, contract, next_block: start_block, log_seq: None });
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let cursor: StakingCursor =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if cursor.chain_id != chain_id || cursor.contract != contract {
            bail!(
                "{} is for contract {} on chain {}, not the configured contract {contract} on chain {chain_id}",
                path.display(),
                cursor.contract,
                cursor.chain_id
            );
        }
        if cursor.log_seq > log_seq {
            bail!(
                "{} has mirrored staking events up to log seq {:?}, but the log ends at seq {:?}",
                path.display(),
                cursor.log_seq,
                log_seq
            );
        }
        Ok(cursor)
    }

    /// Replaces the file at `path` atomically.
    /// Error cases:
    /// - the directory cannot be created, or the file cannot be written, synced, or renamed
    pub fn store(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = crate::custody::cursor::temp_path(path);
        let mut file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        serde_json::to_writer(&mut file, self).context("writing staking cursor")?;
        use std::io::Write;
        file.write_all(b"\n").context("writing staking cursor")?;
        file.sync_all().context("syncing staking cursor")?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }
}

/// Mirrors every staking event in `from..=end` that the engine has not credited, then advances the
/// cursor to `end + 1`. `end` is the header, read through the quorum, of a block at or below `release`,
/// the block the release tag names: staking mirrors have no reversal, so blocks above it wait for finality.
/// The range read is bound to `release` when `end` is below it (`released_staking_events`).
/// Events hold, with the cursor, until `InitStaking` has applied: a mirror the engine would refuse as
/// premature is never sequenced, and a cursor that has not moved only makes the next poll read the same
/// blocks again. The events come from one read over the range, which must match `end`'s hash even when
/// empty. The headers from the lowest block an event is in up to `end` are read through the quorum and
/// must link up to `end`, and each event must be on its block there, so an event from another fork stalls
/// whatever the backends that answered. Each such block is then read again by its hash, so a log backend
/// that lags cannot drop an event from a block it returned another in: pinned reads that differ from the
/// range read in those blocks stall, as do pinned reads that fail.
/// `bound` is whether the poll has verified the configured registry as the staking contract's
/// deployer: while it is false the poll read no staking code at `to`, and since the registry
/// deploys the staking contract atomically no event can legitimately exist in the window, so any
/// event is a contradiction that stalls rather than mirrors. Its second part is whether a configured
/// schedule has been read to name the registry, its third whether a configured vote contract has been
/// read to name the schedule, and its fourth whether a configured fault proof contract has been read to
/// name the vault: a schedule, vote or fault event before then stalls the same way. With `turns`, the
/// bound registry, the progress from `V65` carries the registry's turn state at `to`: the boundary of
/// the drawn turn naming the operator, from `V67` else that of a drawn turn yet to start, or none. A
/// failed event read, by range or by hash, halves `range`; a completed chunk lets it grow back.
/// Error cases:
/// - stalled: a read fails, an event is not on the canonical block there, an event arrives before the
///   registry binding is verified, or the engine refuses a mirror
/// - fatal: the sequencer has stopped, or the cursor cannot be stored
#[allow(clippy::too_many_arguments)]
pub(super) async fn mirror<V: VaultReader>(
    reader: &V,
    tx: &mpsc::Sender<Request>,
    cursor: &mut StakingCursor,
    cursor_path: &Path,
    range: &mut AdaptiveSize,
    (from, end, release): (u64, BlockLink, Option<BlockRef>),
    (bound, schedule_bound, vote_bound, fault_bound): (bool, bool, bool, bool),
    turns: Option<alloy::primitives::Address>,
) -> Result<(), PollError> {
    let (to, block_hash) = (end.number, end.hash);
    if from > to {
        return Ok(());
    }
    let initialised = query(tx, |engine| engine.staking().is_some()).await.map_err(PollError::Fatal)?;
    if !initialised {
        debug!(from, to, "staking is not initialised; holding mirrors until InitStaking applies");
        return Ok(());
    }
    let events = match reader.released_staking_events(from, BlockRef { number: to, hash: block_hash }, release).await {
        Ok(events) => events,
        // A response-size or timeout limit is met with a narrower range on the retry, as for deposits.
        Err(cause) => {
            range.failed();
            return Err(PollError::rpc(cause));
        }
    };
    // A failed read by hash, one a block, narrows the next window and its reads, as a failed range read does.
    if let Err(stalled) = confirm_event_blocks(reader, end, &events).await {
        if matches!(stalled, PollError::Stalled(Stall::Rpc(_))) {
            range.failed();
        }
        return Err(stalled);
    }
    if !bound && !events.is_empty() {
        return Err(PollError::inconsistent(anyhow!(
            "the window names staking events, but the staking contract has no code at its end: the configured contract cannot be the registry's"
        )));
    }
    let from_schedule = |event: &StakingEvent| {
        matches!(event.kind, StakingEventKind::RotationSetAdopted { .. } | StakingEventKind::TurnAssigned { .. })
    };
    if !schedule_bound && events.iter().any(from_schedule) {
        return Err(PollError::inconsistent(anyhow!(
            "the window names schedule events, but the schedule has not been read to name the registry"
        )));
    }
    if !vote_bound && events.iter().any(|event| matches!(event.kind, StakingEventKind::ProposalPassed { .. })) {
        return Err(PollError::inconsistent(anyhow!(
            "the window names vote events, but the vote contract has not been read to name the schedule"
        )));
    }
    if !fault_bound && events.iter().any(|event| matches!(event.kind, StakingEventKind::FaultProven { .. })) {
        return Err(PollError::inconsistent(anyhow!(
            "the window names a proven fault, but the fault proof contract has not been read to name the vault"
        )));
    }
    for event in events {
        mirror_one(tx, event).await?;
    }
    let version = query(tx, |engine| engine.protocol_version()).await.map_err(PollError::Fatal)?;
    if version >= ProtocolVersion::V27 {
        // From V37 the frontier carries its block time, which an elected key waits for.
        let block_time = if version >= ProtocolVersion::V37 { Some(block_time(end)?) } else { None };
        let named_turn = match turns.filter(|_| version >= ProtocolVersion::V65) {
            Some(registry) => {
                // From V67 a drawn turn that has yet to start counts, as the engine accepts it.
                let upcoming = block_time.filter(|_| version >= ProtocolVersion::V67);
                Some(named_turn(reader, registry, to, block_hash, upcoming).await?)
            }
            None => None,
        };
        let progress = Command::StakingProgress { block: to, block_hash, block_time, named_turn };
        let events = submit(tx, progress).await?;
        if events.iter().any(|event| matches!(event, Event::AdminRejected { .. })) {
            return Err(PollError::Stalled(Stall::Refused(anyhow::anyhow!("the engine refused staking progress"))));
        }
    }
    cursor.next_block = to + 1;
    cursor.log_seq = sync(tx).await.map_err(PollError::Fatal)?;
    let stored = cursor.clone();
    let path = cursor_path.to_path_buf();
    tokio::task::spawn_blocking(move || stored.store(&path))
        .await
        .map_err(|e| PollError::Fatal(anyhow::anyhow!("storing the staking cursor: {e}")))?
        .map_err(PollError::Fatal)?;
    range.succeeded();
    debug!(from, to, "staking blocks mirrored");
    Ok(())
}

/// Reads the headers from the lowest block in `events` up to `end`, the range's last block, through the
/// reader, and requires each to name the one below it as its parent and the last to be `end`: a block's
/// hash commits to its parent's, so every event block is then on the chain `end` is on, whatever the
/// backends that answered. Requires each event's block to be the one there, then reads those blocks again,
/// each by its hash, and requires the same events as the range read holds there.
/// Error cases:
/// - stalled: a read fails, the headers do not link up to `end`, an event is not on the block there, or the
///   reads by hash differ
async fn confirm_event_blocks<V: VaultReader>(
    reader: &V,
    end: BlockLink,
    events: &[StakingEvent],
) -> Result<(), PollError> {
    let Some(lowest) = events.iter().map(|event| event.block).min() else { return Ok(()) };
    let hashes = if lowest == end.number {
        BTreeMap::from([(end.number, end.hash)])
    } else {
        let links = reader.headers(lowest, end.number).await.map_err(PollError::rpc)?;
        let mut run = LinkedRun::new(None, lowest);
        run.extend(end.number, &links, |_| false, |_| true).map_err(PollError::unsettled)?;
        if run.last() != Some((end.number, end.hash)) {
            return Err(PollError::unsettled(anyhow!(
                "the headers from staking block {lowest} do not lead to the window's end {} {}",
                end.number,
                end.hash
            )));
        }
        run.window(lowest, end.number)
    };
    let mut blocks = BTreeMap::new();
    for event in events {
        if hashes.get(&event.block) != Some(&event.block_hash) {
            return Err(PollError::unsettled(anyhow!(
                "staking event {} is reported in block {} {}, which is not the canonical block there",
                event.chain_ref(),
                event.block,
                event.block_hash
            )));
        }
        blocks.insert(event.block, event.block_hash);
    }
    let pinned: Vec<_> = blocks.into_iter().map(|(number, hash)| BlockRef { number, hash }).collect();
    let read = reader.pinned_staking_events(&pinned).await.map_err(PollError::rpc)?;
    if read != events {
        return Err(PollError::unsettled(anyhow!(
            "the range read returns {} staking events in {} blocks, but reads by their hashes return {}",
            events.len(),
            pinned.len(),
            read.len()
        )));
    }
    debug!(blocks = pinned.len(), events = read.len(), "staking events confirmed by block hash");
    Ok(())
}

/// Sequences one event's mirror unless the engine has already credited its chain reference.
/// The sequencer alerts a refused mirror as `staking_rejected` or `election_rejected` at Critical;
/// the poll retries it, with the cursor unmoved, on the next pass.
/// Error cases:
/// - stalled: the sequencer refused the mirror (it stays uncredited for the retry)
/// - fatal: the sequencer has stopped
async fn mirror_one(tx: &mpsc::Sender<Request>, event: StakingEvent) -> Result<(), PollError> {
    let span = info_span!("staking", id = %event.chain_ref(), block = event.block, kind = ?event.kind);
    async move {
        let reference = event.chain_ref();
        if query(tx, move |engine| engine.is_staking_credited(&reference)).await.map_err(PollError::Fatal)? {
            debug!("staking event already mirrored");
            return Ok(());
        }
        let events = submit(tx, event.command()).await?;
        if events.iter().any(|event| matches!(event, Event::StakingRejected { .. } | Event::ElectionRejected { .. })) {
            return Err(PollError::Stalled(Stall::Refused(anyhow::anyhow!(
                "the engine refused staking mirror {}",
                event.chain_ref()
            ))));
        }
        match &event.kind {
            StakingEventKind::Staked { account, amount } | StakingEventKind::UnstakeCompleted { account, amount } => {
                info!(%account, amount, "staking event mirrored");
            }
            StakingEventKind::RotationChanged { .. }
            | StakingEventKind::Elected { .. }
            | StakingEventKind::Installed { .. } => {
                info!("operator registry event mirrored");
            }
            StakingEventKind::RotationSetAdopted { set, members, .. } => {
                info!(set, members = members.len(), "rotation set mirrored");
            }
            StakingEventKind::TurnAssigned { turn, position, candidate, .. } => {
                // A turn the engine credits but refuses (`turn_refused`) is not a disagreement.
                info!(turn, position, %candidate, "turn assignment mirrored");
            }
            StakingEventKind::ProposalPassed { proposal, fast, .. } => {
                // A proposal the engine credits but refuses (`proposal_refused`) is not a disagreement.
                info!(proposal, fast, "passed proposal mirrored");
            }
            StakingEventKind::FaultProven { member, turn, position, signer_epoch, seq } => {
                // A fault the engine credits but refuses (`operator_fault_refused`) is not a disagreement.
                info!(%member, turn, position, signer_epoch, seq, "proven operator fault mirrored");
            }
        }
        Ok(())
    }
    .instrument(span)
    .await
}

/// The time of block `end`, in milliseconds, from its header.
/// Error cases:
/// - stalled: the time passes `i64` milliseconds
fn block_time(end: BlockLink) -> Result<Millis, PollError> {
    i64::try_from(end.timestamp).ok().and_then(|seconds| seconds.checked_mul(1000)).ok_or_else(|| {
        PollError::unsettled(anyhow::anyhow!("block {}'s time {} is out of range", end.number, end.timestamp))
    })
}

/// The boundary of the drawn turn `registry` names the operator by in final block `to`, if one does.
/// With none, and `upcoming` the block's time, the boundary of the last turn its schedule has drawn if
/// that is after the block, a turn the registry will name from then.
/// Error cases:
/// - stalled: a read fails, or its answer does not decode
async fn named_turn<V: VaultReader>(
    reader: &V,
    registry: alloy::primitives::Address,
    to: u64,
    hash: B256,
    upcoming: Option<Millis>,
) -> Result<Option<Millis>, PollError> {
    use crate::custody::registry::{decode_last_drawn_turn, decode_named_turn, rotation_state_call};
    let at = BlockRef { number: to, hash };
    let out = reader.contract_call(at, registry, rotation_state_call()).await.map_err(PollError::rpc)?;
    let named = decode_named_turn(&out).map_err(PollError::rpc)?;
    let Some(time) = upcoming.filter(|_| named.is_none()) else {
        return Ok(named);
    };
    let schedule = reader.registry_schedule(at).await.map_err(PollError::rpc)?;
    if schedule.is_zero() {
        return Ok(None);
    }
    let out = reader
        .contract_call(at, schedule, crate::custody::registry::turns_run_until_call())
        .await
        .map_err(PollError::rpc)?;
    Ok(decode_last_drawn_turn(&out).map_err(PollError::rpc)?.filter(|last| *last > time))
}

/// MONKEY base units as the engine's `TokenUnits`, refusing amounts past u128.
pub(crate) fn monkey_units(amount: U256) -> Option<u128> {
    u128::try_from(amount).ok()
}
