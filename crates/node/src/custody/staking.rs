//! Mirroring the staking contract's finalised events into the engine. The deposit watcher calls
//! `mirror` once per confirmed poll, after the same proven window, and only through the
//! Ethereum-finalised boundary the poll proved: a staking mirror has no reversal, so a merely
//! confirmed event must never become a permanent engine balance. Events hold until `InitStaking`
//! has applied, credit once under `tx:logIndex`, and advance the engine's authenticated
//! mirrored-through frontier, so a host cursor cannot skip an event and a restart that re-reads a
//! range mirrors nothing twice.

use std::path::Path;

use alloy::primitives::{B256, U256};
use anyhow::{Context, bail};
use engine_core::command::Command;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::types::AccountId;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::Instrument;
use tracing::{debug, info, info_span};

use crate::custody::credit::{query, submit, sync};
use crate::custody::evm::VaultReader;
use crate::custody::stall::{PollError, Stall};
use crate::sequencer::Request;
/// One staking contract event: a stake, or an unstake the cooldown has passed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StakingEvent {
    pub block: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub log_index: u64,
    pub account: AccountId,
    /// MONKEY base units staked or unstaked.
    pub amount: u128,
    pub kind: StakingEventKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StakingEventKind {
    Staked,
    UnstakeCompleted,
}

impl StakingEvent {
    /// The engine's reference for this event: `<tx hash>:<log index>`, like a deposit's.
    pub fn chain_ref(&self) -> String {
        format!("{}:{}", self.tx_hash, self.log_index)
    }

    /// The mirror command this event carries, naming the finalised block it was read from so the
    /// engine advances the authenticated mirrored-through frontier.
    fn command(&self) -> Command {
        let (account, amount, block, chain_ref) = (self.account, TokenUnits(self.amount), self.block, self.chain_ref());
        match self.kind {
            StakingEventKind::Staked => Command::Stake { account, amount, block, chain_ref },
            StakingEventKind::UnstakeCompleted => Command::Unstake { account, amount, block, chain_ref },
        }
    }
}

/// The watcher's place in the staking log, kept at `custody.evm.staking.cursor_path`. It only
/// avoids rescanning before the first mirror: from then on the poll re-anchors `next_block` from
/// the engine's mirrored-through frontier, so a host-edited file can make the watcher re-read
/// blocks, never skip them; the engine's credited references stop a double mirror. `log_seq`
/// refuses a cursor ahead of the recovered log, as the deposit cursor's does.
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

/// Mirrors every staking event in `from..=to` that the engine has not credited, then advances the
/// cursor to `to + 1`. `to` is the Ethereum-finalised boundary the poll proved: staking mirrors
/// have no reversal, so blocks above it wait for finality. Events hold, with the cursor, until
/// `InitStaking` has applied: a mirror the engine would refuse as premature is never sequenced,
/// and a cursor that has not moved only makes the next poll read the same blocks again. The
/// caller proves the range's blocks before calling, and passes their hashes; an event on an
/// unproven block stalls the poll.
/// Error cases:
/// - stalled: a read fails, an event is on an unproven block, or the engine refuses a mirror
/// - fatal: the sequencer has stopped, or the cursor cannot be stored
pub(super) async fn mirror<V: VaultReader>(
    reader: &V,
    tx: &mpsc::Sender<Request>,
    cursor: &mut StakingCursor,
    cursor_path: &Path,
    from: u64,
    to: u64,
    hashes: &std::collections::BTreeMap<u64, B256>,
) -> Result<(), PollError> {
    if from > to {
        return Ok(());
    }
    let initialised = query(tx, |engine| engine.staking().is_some()).await.map_err(PollError::Fatal)?;
    if !initialised {
        debug!(from, to, "staking is not initialised; holding mirrors until InitStaking applies");
        return Ok(());
    }
    // The engine's frontier, not the host cursor, anchors where mirroring resumes: credited
    // references stop a double mirror, and a file that claims more than the frontier is re-read,
    // never skipped.
    let frontier = query(tx, |engine| engine.staking().and_then(|staking| staking.mirrored_through))
        .await
        .map_err(PollError::Fatal)?;
    let from = frontier.map_or(from, |frontier| frontier.saturating_add(1).max(from));
    if from > to {
        // The finalised boundary sits at or behind the mirrored frontier (its tag reorganised, or
        // a lagging cursor caught up): nothing mirrors this poll, and the cursor names the first
        // unmirrored block so the next poll starts there.
        cursor.next_block = from;
        return Ok(());
    }
    let events = reader.staking_events(from, to).await.map_err(PollError::rpc)?;
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
    for event in events {
        mirror_one(tx, event).await?;
    }
    cursor.next_block = to + 1;
    cursor.log_seq = sync(tx).await.map_err(PollError::Fatal)?;
    let stored = cursor.clone();
    let path = cursor_path.to_path_buf();
    tokio::task::spawn_blocking(move || stored.store(&path))
        .await
        .map_err(|e| PollError::Fatal(anyhow::anyhow!("storing the staking cursor: {e}")))?
        .map_err(PollError::Fatal)?;
    debug!(from, to, "staking blocks mirrored");
    Ok(())
}

/// Sequences one event's mirror unless the engine has already credited its chain reference.
/// The sequencer alerts a refused mirror as `staking_rejected` at Critical; the poll retries it,
/// with the cursor unmoved, on the next pass.
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
        if events.iter().any(|event| matches!(event, Event::StakingRejected { .. })) {
            return Err(PollError::Stalled(Stall::Refused(anyhow::anyhow!(
                "the engine refused staking mirror {}",
                event.chain_ref()
            ))));
        }
        info!(account = %event.account, amount = %event.amount, ?event.kind, "staking event mirrored");
        Ok(())
    }
    .instrument(span)
    .await
}

/// MONKEY base units as the engine's `TokenUnits`, refusing amounts past u128.
pub(crate) fn monkey_units(amount: U256) -> Option<u128> {
    u128::try_from(amount).ok()
}
