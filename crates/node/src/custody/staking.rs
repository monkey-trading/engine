//! Mirroring the staking contract's finalised events, and from `V37` the operator registry's, into
//! the engine. The deposit watcher calls `mirror` once per confirmed poll, after the same proven
//! window, and only through the Ethereum-finalised boundary the poll proved: a staking mirror has
//! no reversal, so a merely confirmed event must never become a permanent engine balance or
//! election. Events hold until `InitStaking` has applied, credit once under `tx:logIndex`, and
//! advance the engine's authenticated mirrored-through frontier, so a host cursor cannot skip an
//! event and a restart that re-reads a range mirrors nothing twice.

use std::path::Path;

use alloy::primitives::{B256, U256};
use anyhow::{Context, anyhow, bail};
use engine_core::command::Command;
use engine_core::election::RegistryElection;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::Instrument;
use tracing::{debug, info, info_span};

use crate::custody::credit::{query, submit, sync};
use crate::custody::evm::VaultReader;
use crate::custody::sizing::AdaptiveSize;
use crate::custody::stall::{PollError, Stall};
use crate::sequencer::Request;
/// One mirrorable event: from the staking contract, a stake or an unstake the cooldown has passed;
/// from the operator registry, a rotation switch or an election.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StakingEvent {
    pub block: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub log_index: u64,
    pub kind: StakingEventKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        match self.kind {
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

/// Mirrors every staking event in `window` (`from..=to`) that the engine has not credited, then
/// advances the cursor to `to + 1`. `to` is the Ethereum-finalised boundary the poll proved: staking mirrors
/// have no reversal, so blocks above it wait for finality. Events hold, with the cursor, until
/// `InitStaking` has applied: a mirror the engine would refuse as premature is never sequenced,
/// and a cursor that has not moved only makes the next poll read the same blocks again. The
/// caller proves the range's blocks before calling, and passes their hashes. The event response
/// must match the proven endpoint even when empty; an event on an unproven block also stalls.
/// `bound` is whether the poll has verified the configured registry as the staking contract's
/// deployer: while it is false the poll read no staking code at `to`, and since the registry
/// deploys the staking contract atomically no event can legitimately exist in the window, so any
/// event is a contradiction that stalls rather than mirrors. A failed event read halves `range`;
/// a completed chunk lets it grow back.
/// Error cases:
/// - stalled: a read fails, an event is on an unproven block, an event arrives before the
///   registry binding is verified, or the engine refuses a mirror
/// - fatal: the sequencer has stopped, or the cursor cannot be stored
#[allow(clippy::too_many_arguments)]
pub(super) async fn mirror<V: VaultReader>(
    reader: &V,
    tx: &mpsc::Sender<Request>,
    cursor: &mut StakingCursor,
    cursor_path: &Path,
    range: &mut AdaptiveSize,
    window: std::ops::RangeInclusive<u64>,
    hashes: &std::collections::BTreeMap<u64, B256>,
    bound: bool,
) -> Result<(), PollError> {
    let (from, to) = (*window.start(), *window.end());
    if from > to {
        return Ok(());
    }
    let initialised = query(tx, |engine| engine.staking().is_some()).await.map_err(PollError::Fatal)?;
    if !initialised {
        debug!(from, to, "staking is not initialised; holding mirrors until InitStaking applies");
        return Ok(());
    }
    let block_hash = *hashes
        .get(&to)
        .ok_or_else(|| PollError::unsettled(anyhow::anyhow!("staking progress lacks its proven endpoint")))?;
    let blocks = (from..=to)
        .map(|number| {
            hashes
                .get(&number)
                .copied()
                .map(|hash| crate::custody::evm::BlockRef { number, hash })
                .ok_or_else(|| PollError::unsettled(anyhow::anyhow!("staking range lacks a proven block")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let events = match reader.staking_events(&blocks).await {
        Ok(events) => events,
        // A response-size or timeout limit is met with a narrower range on the retry, as for deposits.
        Err(cause) => {
            range.failed();
            return Err(PollError::rpc(cause));
        }
    };
    for event in &events {
        if hashes.get(&event.block) != Some(&event.block_hash) {
            return Err(PollError::unsettled(anyhow::anyhow!(
                "staking event {} is reported in block {} {}, which is not the proven block there",
                event.chain_ref(),
                event.block,
                event.block_hash
            )));
        }
    }
    if !bound && !events.is_empty() {
        return Err(PollError::inconsistent(anyhow!(
            "the window names staking events, but the staking contract has no code at its end: the configured contract cannot be the registry's"
        )));
    }
    for event in events {
        mirror_one(tx, event).await?;
    }
    let version = query(tx, |engine| engine.protocol_version()).await.map_err(PollError::Fatal)?;
    if version >= ProtocolVersion::V27 {
        // From V37 the frontier carries its block time, which an elected key waits for.
        let block_time =
            if version >= ProtocolVersion::V37 { Some(block_time(reader, to, block_hash).await?) } else { None };
        let events = submit(tx, Command::StakingProgress { block: to, block_hash, block_time }).await?;
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
        match event.kind {
            StakingEventKind::Staked { account, amount } | StakingEventKind::UnstakeCompleted { account, amount } => {
                info!(%account, amount, "staking event mirrored");
            }
            StakingEventKind::RotationChanged { .. } | StakingEventKind::Elected { .. } => {
                info!("operator registry event mirrored");
            }
        }
        Ok(())
    }
    .instrument(span)
    .await
}

/// The time of proven block `to`, in milliseconds, from its header read through the reader.
/// Error cases:
/// - stalled: the read fails, answers for another block or hash, or the time passes `i64` milliseconds
async fn block_time<V: VaultReader>(reader: &V, to: u64, hash: B256) -> Result<Millis, PollError> {
    let links = reader.headers(to, to).await.map_err(PollError::rpc)?;
    let [link] = links.as_slice() else {
        return Err(PollError::unsettled(anyhow::anyhow!("staking progress has no header at block {to}")));
    };
    if link.number != to || link.hash != hash {
        return Err(PollError::unsettled(anyhow::anyhow!("block {to}'s header is not the proven block")));
    }
    i64::try_from(link.timestamp)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1000))
        .ok_or_else(|| PollError::unsettled(anyhow::anyhow!("block {to}'s time {} is out of range", link.timestamp)))
}

/// MONKEY base units as the engine's `TokenUnits`, refusing amounts past u128.
pub(crate) fn monkey_units(amount: U256) -> Option<u128> {
    u128::try_from(amount).ok()
}
