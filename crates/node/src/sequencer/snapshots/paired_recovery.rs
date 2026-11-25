//! Normal paired-cache recovery reads indexed metadata and the physical suffix, never history rows.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use engine_core::types::AccountId;
use tracing::{error, info};

use crate::authenticated::Store;
use crate::config::{HistorySection, NodeConfig, SnapshotSection};
use crate::daily_snapshots::Signed;
use crate::history::authenticated::Index;
use crate::history::rebuild::Fingerprinter;
use crate::history::startup::{Catalog, Plan, Resume, Source};
use crate::log::catalog_access::{CatalogRead, StoreCatalogRead};
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent, LogEntry, SegmentedLog};
use crate::sequencer::replay::Replay;
use crate::sequencer::{ReplayedLog, Sequencer};
use crate::snapshot::head::{HeadStatement, StoreId};
use crate::snapshot::paired::PrefixId;
use crate::snapshot::{self, RecoveryBase, head};

/// The signed parent chain from a cache's base to the vault's finalized anchor at or past it, which a
/// start from the cache checks over the suffix it replays, as a full start's signer history checks it
/// over the whole log: each signed checkpoint up to the anchor names the one before it, from the base
/// on, and the last of them is the anchor.
struct AnchorChain {
    anchor: CheckpointParent,
    tip: CheckpointParent,
}

impl Sequencer {
    /// Plans paired recovery or a strict log-derived migration without any persisted mutation:
    /// `read_paired_cache`, then `replay_paired_log` with `authorities` when no cache verifies.
    /// Error cases:
    /// - as `read_paired_cache` or `replay_paired_log`
    #[cfg(test)]
    pub(crate) fn read_paired_log(
        cfg: &NodeConfig,
        signer: Option<AccountId>,
        parent: Option<CheckpointParent>,
        authorities: crate::checkpoints::SignerHistory,
        stats: Option<crate::stats::Stats>,
        capture: Option<crate::log_heads::Head>,
        mut observe: impl FnMut(&LogEntry),
    ) -> Result<(ReplayedLog, Plan)> {
        match Self::read_paired_cache(cfg, signer, parent, stats.as_ref(), capture, None, &mut observe)? {
            Some(recovered) => Ok(recovered),
            None => Self::replay_paired_log(cfg, signer, parent, authorities, stats, capture, None, observe),
        }
    }

    /// Plans paired recovery from the newest cache that verifies, without any persisted mutation, or
    /// `None` when none does, each one rejected with a Warning. `capture`, when set, is the on-chain
    /// posted head: the replayed suffix records the cumulative command-prefix digest at its seq for
    /// the posted-head check, and a posted seq at or under the snapshot base is bound to the snapshot's
    /// history by the commands since it before the snapshot is accepted (a refusal is fatal, not a
    /// candidate rejection). `daily`, a signed daily snapshot at or under the vault's finalized checkpoint,
    /// starts the pass over the log's prefix a cache's check makes with no head posted.
    /// Error cases:
    /// - snapshots are absent, or the posted head conflicts with the log below a cache's base
    pub(crate) fn read_paired_cache(
        cfg: &NodeConfig,
        signer: Option<AccountId>,
        parent: Option<CheckpointParent>,
        stats: Option<&crate::stats::Stats>,
        capture: Option<crate::log_heads::Head>,
        daily: Option<&Signed>,
        observe: &mut impl FnMut(&LogEntry),
    ) -> Result<Option<(ReplayedLog, Plan)>> {
        let snapshots = cfg.snapshots.as_ref().context("paired recovery requires snapshots config")?;
        let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
        // No cache can pass without a pin, so none is read and none raises a Warning.
        let candidates = if snapshots.trusted_signers.is_empty() {
            info!("no snapshot signer is pinned; recovering by full replay");
            Ok(Vec::new())
        } else {
            snapshot::candidates_readonly(snapshots)
        };
        match candidates {
            Ok(paths) => {
                for path in paths {
                    let checks = (signer, parent, capture, daily);
                    match candidate(cfg, snapshots, &path, domain, checks, stats, observe) {
                        Ok(recovered) => return Ok(Some(recovered)),
                        // A posted head the log does not hold refuses startup; it is not a cache to
                        // reject and fall back from — the fallback would serve the conflicting books.
                        Err(error) if error.is::<crate::log_heads::prefix::Rollback>() => return Err(error),
                        Err(error) => snapshot::rejected(&path, &error),
                    }
                }
            }
            Err(error) => snapshot::rejected(&snapshots.dir, &error),
        }
        Ok(None)
    }

    /// The replay a paired recovery falls back to when no cache verifies, planning a strict log-derived
    /// migration without any persisted mutation: of the log after `daily`'s checkpoint, a signed daily
    /// snapshot at or under the vault's finalized checkpoint, when one is given, into the dashboard record `stats`
    /// restored from the one it carries, or else of the whole log, into `stats` started over.
    /// `capture` is as `read_paired_cache`'s; a head posted at or under the daily snapshot's checkpoint is
    /// bound to the history it carries (`prefix::under_daily`) before the replay.
    /// Error cases:
    /// - snapshots are absent, or the replay cannot verify the authoritative log
    /// - the replay cannot go on from the daily snapshot at all (`SnapshotRefused`), before it reads an entry
    /// - the posted head is not in the daily snapshot's history (`prefix::under_daily`)
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replay_paired_log(
        cfg: &NodeConfig,
        signer: Option<AccountId>,
        parent: Option<CheckpointParent>,
        authorities: crate::checkpoints::SignerHistory,
        stats: Option<crate::stats::Stats>,
        capture: Option<crate::log_heads::Head>,
        mut daily: Option<Signed>,
        mut observe: impl FnMut(&LogEntry),
    ) -> Result<(ReplayedLog, Plan)> {
        let snapshots = cfg.snapshots.as_ref().context("paired recovery requires snapshots config")?;
        let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
        let mut replay = parent.map_or_else(
            || Replay::with_trust(Some(domain), signer),
            |parent| Replay::with_recovery(Some(domain), signer, parent),
        );
        // A snapshot holds only checkpoints the current key signed; full replay authenticates the rest.
        replay.set_authorities(authorities)?;
        if let Some(posted) = capture {
            replay.capture_link_at(posted.seq);
        }
        // From a daily snapshot, the record goes on from the one it carries, as a start from one without
        // `[snapshots]` does, or starts over there when it carries none.
        match (stats, daily.as_mut()) {
            (Some(stats), Some(daily)) => {
                match crate::daily_snapshots::History::read(daily.snapshot.history.take().as_deref()).stats {
                    Some(record) => stats.restore(record),
                    None => stats.restart(),
                }
                replay.record_stats_continuing(stats);
            }
            (Some(stats), None) => replay.record_stats_with(stats),
            (None, _) => {}
        }
        let (dir, seals) = (&cfg.node.log_dir, crate::log::SealChecks::Every(cfg.node.replay_workers));
        let posted_ts = match &daily {
            Some(daily) => crate::log_heads::prefix::under_daily(dir, capture, daily)?,
            None => None,
        };
        let snapshot_seq = daily.as_ref().map(|daily| daily.snapshot.offset.seq);
        let mut fingerprint = Fingerprinter::after(daily.as_ref().map(|daily| daily.snapshot.offset));
        let (replay, log) = match daily {
            Some(daily) => replay.replay_after(daily, dir, seals, |entry| {
                fingerprint.observe(entry)?;
                observe(entry);
                Ok(())
            })?,
            None => {
                replay.check_ahead(dir, seals);
                let log = SegmentedLog::open_ahead(dir, |entry| {
                    replay.apply(&entry)?;
                    fingerprint.observe(&entry)?;
                    observe(&entry);
                    Ok(())
                });
                let log = replay.settled(log)?;
                (replay, log)
            }
        };
        let mut replayed = replay.finish();
        replayed.ts_at = replayed.ts_at.or(posted_ts);
        let plan = plan(cfg, snapshots, domain, Source::Rebuild { prefix: fingerprint.finish() }, Catalog::Rebuild);
        Ok((ReplayedLog { log: log.into(), replayed, snapshot_seq, legacy_authorizations: Default::default() }, plan))
    }
}

impl AnchorChain {
    /// The chain from a signed `base` to `anchor`, when `anchor` is a checkpoint at or past it.
    /// Error cases:
    /// - the base's root is not a hash
    fn new(base: &Checkpoint, anchor: Option<CheckpointParent>) -> Result<Option<Self>> {
        let anchor = anchor.filter(|anchor| anchor.has_checkpoint && anchor.seq >= base.seq && base.parent.is_some());
        anchor.map(|anchor| Ok(Self { anchor, tip: link(base)? })).transpose()
    }

    /// Takes the suffix's next entry.
    /// Error cases:
    /// - a signed checkpoint up to the anchor names another parent, or its root is not a hash
    fn take(&mut self, entry: &LogEntry) -> Result<()> {
        if let LogEntry::Checkpoint(checkpoint) = entry
            && checkpoint.parent.is_some()
            && checkpoint.seq <= self.anchor.seq
        {
            ensure!(
                checkpoint.parent == Some(self.tip),
                "checkpoint at seq {} breaks the signed parent chain",
                checkpoint.seq
            );
            self.tip = link(checkpoint)?;
        }
        Ok(())
    }

    /// Ends the chain where the suffix ends.
    /// Error cases:
    /// - the suffix's chain does not end at the anchor
    fn finish(self) -> Result<()> {
        ensure!(self.tip == self.anchor, "signed parent chain does not end at the finalized vault anchor");
        Ok(())
    }
}

/// Accepts no later head as checkpoint C's paired history; a head before C is bound to C by the commands
/// between them, and history resumes from it and indexes them again. Every suffix entry still passes
/// current strict Replay.
/// A posted head at or under C is bound to C's history by the commands since it before the
/// snapshot is accepted; that refusal (see `log_heads::prefix::Rollback`) aborts recovery rather
/// than falling back, so a forked prefix cannot be served from the snapshot. With `[log_heads]` and
/// no head posted, a pass over the on-disk prefix, from `daily`'s checkpoint when a signed daily snapshot
/// under C is given, stamps a vault-finalized checkpoint under C. With a finalized checkpoint at or past C,
/// the suffix's signed parent chain from C to it is checked (`AnchorChain`).
/// Error cases:
/// - certificate, fixed metadata, prefix extension, the paired head's binding to C, boundary bytes, replay,
///   the posted-head prefix
///   check, the parent chain to the finalized checkpoint, or the anchor-stamping pass, or availability
///   fails
fn candidate(
    cfg: &NodeConfig,
    snapshots: &SnapshotSection,
    path: &Path,
    domain: CheckpointDomain,
    (signer, parent, capture, daily): (
        Option<AccountId>,
        Option<CheckpointParent>,
        Option<crate::log_heads::Head>,
        Option<&Signed>,
    ),
    stats: Option<&crate::stats::Stats>,
    observe: &mut impl FnMut(&LogEntry),
) -> Result<(ReplayedLog, Plan)> {
    let (snapshot, engine) = snapshot::read(snapshots, path, domain.chain_id, domain.vault)?;
    let at = snapshot.offset;
    // The snapshot authenticates the state at its base; the on-chain posted head is independent
    // evidence. A posted seq at or under the base is bound to the snapshot's history by hashing the
    // commands after it, through the base, onto its link, which must give the snapshot's certified
    // `command_prefix`, the one `from_snapshot` seeds — no engine apply and no read before the head,
    // so snapshot recovery keeps its savings. The time it gives is carried to the stall alarm.
    // With `[log_heads]` and no head posted, a pass over the on-disk prefix stamps a vault-finalized
    // checkpoint under the base, the alarm's anchor then; the suffix replay stamps one past the base.
    let posted = capture.filter(|posted| posted.seq <= at.seq);
    let anchor = parent
        .filter(|parent| cfg.log_heads.is_some() && parent.has_checkpoint && parent.seq < at.seq && capture.is_none());
    let (mut posted_ts, mut anchor_ts) = (None, None);
    let (dir, certified) = (&cfg.node.log_dir, snapshot.context.command_prefix);
    if let Some(posted) = posted {
        posted_ts = Some(crate::log_heads::prefix::since(dir, posted, at.seq, engine.now(), certified)?);
    } else if let Some(anchor) = anchor {
        let trusted = signer.context("snapshot recovery needs the trusted signer to verify the log's prefix")?;
        let (workers, start) = (cfg.node.replay_workers, daily.and_then(|daily| start_of(daily, anchor.seq)));
        let verified = crate::log_heads::prefix::verify_from(
            dir,
            (domain, trusted),
            (anchor.seq, at.seq),
            certified,
            workers,
            start,
        )?;
        anchor_ts = Some(verified.anchor_ts);
    }
    let pair = snapshot.pair.as_ref().context("engine-only cache requires full history migration")?;
    let expected = PrefixId {
        domain,
        offset: at,
        checkpoint: snapshot.checkpoint.clone(),
        command_prefix: snapshot.context.command_prefix,
        ts: engine.now(),
    };
    let history_c = pair.verify(snapshots, &expected, cfg.history.retention_ms()?)?;
    // A pair's history head before C is the cache's own history only if the commands between them carry its
    // prefix to C's: both are signed.
    if let Some(seq) = history_c.head.last_seq().filter(|seq| *seq < at.seq) {
        crate::log_heads::prefix::bind_history(dir, (seq, history_c.head.command_prefix), at.seq, certified)?;
    }
    let mut stores = BTreeMap::new();
    let store_c = open_store(snapshots, history_c.store, &mut stores)?;
    Index::resume(store_c.clone(), history_c.head.clone())?;
    let catalog_id = pair.catalog_store;
    let catalog_store = open_store(snapshots, catalog_id, &mut stores)?;
    let catalog_tip = crate::snapshot::catalog_heads::read_latest(snapshots, catalog_id, &pair.catalog.domain)?;
    // Without a retained head that resumes (each one rejected with a Warning), the pair's own
    // head at C still carries history.
    let history_h = match complete_head(snapshots, domain, at.seq, cfg.history.retention_ms()?, &mut stores) {
        Ok(history_h) => {
            ensure!(history_h.store != catalog_id, "complete history head shares the catalog generation");
            let history_store = open_store(snapshots, history_h.store, &mut stores)?;
            Index::resume(history_store.clone(), history_h.head.clone())?;
            Some((history_h, history_store))
        }
        Err(_) => None,
    };
    let h_seq = history_h.as_ref().and_then(|(history_h, _)| history_h.head.last_seq());
    let boundary = pair.catalog.clone();
    let mut chain = AnchorChain::new(&snapshot.checkpoint, parent)?;
    let mut replay = Replay::from_snapshot(engine, snapshot, domain, signer, parent)?;
    // A start from a cache with no replay state of its own would fold none, so it would publish no daily snapshot,
    // and no later cache would carry the state either: a start that is not from such a cache seeds the fold again.
    ensure!(
        cfg.daily_snapshots.is_none() || replay.folds(),
        "the cache carries no replay state of its own, which the daily snapshots need"
    );
    if let Some(posted) = capture {
        replay.capture_link_at(posted.seq);
    }
    if let Some(stats) = stats {
        replay.record_stats_with(stats.clone());
    }
    let mut matched_h = h_seq == Some(at.seq);
    if let Some((history_h, _)) = history_h.as_ref().filter(|_| matched_h) {
        replay.verify_history_head(&history_h.head)?;
    }
    let probe = catalog_store.clone();
    let read: Arc<dyn CatalogRead> = Arc::new(StoreCatalogRead::new(
        Arc::new(catalog_store.clone()),
        Arc::new(move |root| probe.ensure_current(root)),
    ));
    replay.check_ahead(&cfg.node.log_dir, crate::log::SealChecks::Every(cfg.node.replay_workers));
    let suffix = SegmentedLog::open_suffix(&cfg.node.log_dir, read.clone(), &boundary, catalog_tip.as_ref(), |entry| {
        replay.apply(&entry)?;
        chain.as_mut().map(|chain| chain.take(&entry)).transpose()?;
        if let (LogEntry::Command(command), Some((history_h, _))) = (&entry, &history_h)
            && Some(command.sequenced.seq) == h_seq
        {
            replay.verify_history_head(&history_h.head)?;
            matched_h = true;
        }
        Ok(())
    });
    let mut log = replay.settled(suffix)?;
    chain.map(AnchorChain::finish).transpose()?;
    let mut replayed = replay.finish();
    replayed.ts_at = replayed.ts_at.or(posted_ts);
    replayed.anchor_ts = replayed.anchor_ts.or(anchor_ts);
    replayed.origin.require_staking_provenance()?;
    // A signed history head the replay never reached, or a catalog tip covering what the log no
    // longer holds, proves the node took commands the log lost. Both take only synced entries, so
    // storage lost them or the host truncated the log. The tip may cover the lost tail even when
    // only history shows it, so the catalog resumes from C whenever either does.
    let history_lost = history_h.is_some() && !matched_h;
    if history_lost {
        log.rewind_catalog(read, &boundary)?;
    }
    if log.catalog_rewound() {
        error!(
            alert = "log_tail_lost",
            log_seq = ?replayed.engine.last_seq(),
            "the log ends before commands signed heads cover (history head at {:?}); acknowledged commands are lost",
            h_seq.filter(|_| history_lost)
        );
    }
    // History resumes from the pair's own head, at C or bound to it above, when no retained head resumes
    // or the log ends before the newest one; activation discards the later versions and history indexes
    // the rest of the log again, from the head. The catalog resumes from C.
    let (history_id, history_store, history_head) = match history_h {
        Some((history_h, history_store)) if matched_h => (history_h.store, history_store, history_h.head),
        _ => {
            ensure!(history_c.store != catalog_id, "paired history head shares the catalog generation");
            (history_c.store, store_c, history_c.head)
        }
    };
    let resume = match catalog_tip {
        Some(tip) if !log.catalog_rewound() => Resume::Tip(Box::new(tip)),
        _ => Resume::Boundary(boundary.catalog.clone()),
    };
    // The finalized witness can precede C; it is concrete signed context, not a digest surrogate.
    if let Some(RecoveryBase::Checkpoint { checkpoint }) = &replayed.recovery_base {
        observe(&LogEntry::Checkpoint(checkpoint.clone()));
    }
    let plan = plan(
        cfg,
        snapshots,
        domain,
        Source::Complete { id: history_id, store: history_store, head: history_head },
        Catalog::Complete { id: catalog_id, store: catalog_store, resume },
    );
    Ok((
        ReplayedLog {
            log: log.into(),
            replayed,
            snapshot_seq: Some(at.seq),
            legacy_authorizations: Default::default(),
        },
        plan,
    ))
}

/// Reads only a bounded set of signed latest-head metadata files and fixed proof keys. The store
/// may hold later versions the owner committed before a crash stopped it from signing them;
/// activation discards those after preflight.
/// Error cases:
/// - no independently authenticated complete compatible head is available
fn complete_head(
    cfg: &SnapshotSection,
    domain: CheckpointDomain,
    checkpoint_seq: u64,
    retention_ms: engine_core::types::Millis,
    stores: &mut BTreeMap<StoreId, Store>,
) -> Result<HeadStatement> {
    for path in head::candidates_readonly(cfg)? {
        let candidate = (|| {
            let statement = head::read(cfg, &path, domain)?;
            ensure!(
                statement.head.last_seq().is_some_and(|seq| seq >= checkpoint_seq),
                "history head precedes checkpoint"
            );
            ensure!(statement.head.retention_ms == retention_ms, "history head retention changed");
            let store = open_store(cfg, statement.store, stores)?;
            Index::resume(store.clone(), statement.head.clone())?;
            Ok(statement)
        })();
        match candidate {
            Ok(statement) => return Ok(statement),
            Err(error) => head::rejected(&path, &error),
        }
    }
    anyhow::bail!("no complete compatible history head")
}

fn open_store(cfg: &SnapshotSection, id: StoreId, stores: &mut BTreeMap<StoreId, Store>) -> Result<Store> {
    if let Some(store) = stores.get(&id) {
        return Ok(store.clone());
    }
    let store = Store::open_existing(id.path(&cfg.dir))?;
    stores.insert(id, store.clone());
    Ok(store)
}

fn plan(
    cfg: &NodeConfig,
    snapshots: &SnapshotSection,
    domain: CheckpointDomain,
    source: Source,
    catalog: Catalog,
) -> Plan {
    Plan {
        snapshots: snapshots.clone(),
        history: HistorySection { dir: cfg.history.dir.clone(), retention: cfg.history.retention, marks: None },
        log_dir: cfg.node.log_dir.clone(),
        domain,
        source,
        catalog,
        daily: cfg.daily_snapshots.as_ref().map(|daily| daily.dir.clone()),
    }
}

/// Where the pass over a log's prefix to the vault's finalized checkpoint at `anchor` starts in place of the
/// log's start: `daily`'s checkpoint, when it is at or under the anchor and carries its replay state.
fn start_of(daily: &Signed, anchor: u64) -> Option<crate::log_heads::prefix::SnapshotStart<'_>> {
    let snapshot = &daily.snapshot;
    let carried = snapshot.carried.as_ref().filter(|_| snapshot.offset.seq <= anchor)?;
    Some(crate::log_heads::prefix::SnapshotStart {
        offset: snapshot.offset,
        checkpoint: &snapshot.checkpoint,
        command_prefix: carried.command_prefix.into(),
        ts: snapshot.engine.now(),
    })
}

/// The parent link a checkpoint's child names.
/// Error cases:
/// - its root is not a hash
fn link(checkpoint: &Checkpoint) -> Result<CheckpointParent> {
    Ok(CheckpointParent { has_checkpoint: true, seq: checkpoint.seq, root: checkpoint.root.parse()? })
}

#[cfg(test)]
mod tests;
