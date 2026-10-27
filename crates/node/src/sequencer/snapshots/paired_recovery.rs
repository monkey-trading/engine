//! Normal paired-cache recovery reads indexed metadata and the physical suffix, never history rows.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use engine_core::types::AccountId;
use tracing::{error, info};

use crate::authenticated::Store;
use crate::config::{HistorySection, NodeConfig, SnapshotSection};
use crate::history::authenticated::Index;
use crate::history::rebuild::Fingerprinter;
use crate::history::startup::{Catalog, Plan, Resume, Source};
use crate::log::catalog_access::{CatalogRead, StoreCatalogRead};
use crate::log::{CheckpointDomain, CheckpointParent, LogEntry, SegmentedLog};
use crate::sequencer::replay::Replay;
use crate::sequencer::{ReplayedLog, Sequencer};
use crate::snapshot::head::{HeadStatement, StoreId};
use crate::snapshot::paired::PrefixId;
use crate::snapshot::{self, RecoveryBase, head};

impl Sequencer {
    /// Plans paired recovery or a strict log-derived migration without any persisted mutation.
    /// Error cases:
    /// - snapshots are absent, or no cache/full replay can verify the authoritative log
    pub(crate) fn read_paired_log(
        cfg: &NodeConfig,
        signer: Option<AccountId>,
        parent: Option<CheckpointParent>,
        authorities: crate::checkpoints::SignerHistory,
        mut observe: impl FnMut(&LogEntry),
    ) -> Result<(ReplayedLog, Plan)> {
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
                    match candidate(cfg, snapshots, &path, domain, signer, parent, &mut observe) {
                        Ok(recovered) => return Ok(recovered),
                        Err(error) => snapshot::rejected(&path, &error),
                    }
                }
            }
            Err(error) => snapshot::rejected(&snapshots.dir, &error),
        }
        let mut replay = parent.map_or_else(
            || Replay::with_trust(Some(domain), signer),
            |parent| Replay::with_recovery(Some(domain), signer, parent),
        );
        // A snapshot holds only checkpoints the current key signed; full replay authenticates the rest.
        replay.set_authorities(authorities)?;
        let mut fingerprint = Fingerprinter::new();
        let log = SegmentedLog::open(&cfg.node.log_dir, |entry| {
            replay.apply(&entry)?;
            fingerprint.observe(&entry)?;
            observe(&entry);
            Ok(())
        })?;
        let replayed = replay.finish();
        let plan =
            plan(cfg, snapshots, domain, signer, Source::Rebuild { prefix: fingerprint.finish() }, Catalog::Rebuild);
        Ok((ReplayedLog { log, replayed, snapshot_seq: None, legacy_authorizations: Default::default() }, plan))
    }
}

/// Accepts no later head as checkpoint C; every suffix entry still passes current strict Replay.
/// Error cases:
/// - certificate, fixed metadata, prefix extension, boundary bytes, replay or availability fails
fn candidate(
    cfg: &NodeConfig,
    snapshots: &SnapshotSection,
    path: &Path,
    domain: CheckpointDomain,
    signer: Option<AccountId>,
    parent: Option<CheckpointParent>,
    observe: &mut impl FnMut(&LogEntry),
) -> Result<(ReplayedLog, Plan)> {
    let (snapshot, engine) = snapshot::read(snapshots, path, domain.chain_id, domain.vault)?;
    let at = snapshot.offset;
    let pair = snapshot.pair.as_ref().context("engine-only cache requires full history migration")?;
    let expected = PrefixId {
        domain,
        offset: at,
        checkpoint: snapshot.checkpoint.clone(),
        command_prefix: snapshot.context.command_prefix,
        ts: engine.now(),
    };
    let history_c = pair.verify(snapshots, &expected, cfg.history.retention_ms()?)?;
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
    let mut replay = Replay::from_snapshot(engine, snapshot, domain, signer, parent)?;
    let mut matched_h = h_seq == Some(at.seq);
    if let Some((history_h, _)) = history_h.as_ref().filter(|_| matched_h) {
        replay.verify_history_head(&history_h.head)?;
    }
    let probe = catalog_store.clone();
    let read: Arc<dyn CatalogRead> = Arc::new(StoreCatalogRead::new(
        Arc::new(catalog_store.clone()),
        Arc::new(move |root| probe.ensure_current(root)),
    ));
    let mut log =
        SegmentedLog::open_suffix(&cfg.node.log_dir, read.clone(), &boundary, catalog_tip.as_ref(), |entry| {
            replay.apply(&entry)?;
            if let (LogEntry::Command(command), Some((history_h, _))) = (&entry, &history_h)
                && Some(command.sequenced.seq) == h_seq
            {
                replay.verify_history_head(&history_h.head)?;
                matched_h = true;
            }
            Ok(())
        })?;
    let replayed = replay.finish();
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
    // History resumes from the pair's own head at C, which matches the replay by construction,
    // when no retained head resumes or the log ends before the newest one; activation discards the
    // later versions and history indexes the rest of the log again. The catalog does the same.
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
        signer,
        Source::Complete { id: history_id, store: history_store, head: history_head },
        Catalog::Complete { id: catalog_id, store: catalog_store, resume },
    );
    Ok((ReplayedLog { log, replayed, snapshot_seq: Some(at.seq), legacy_authorizations: Default::default() }, plan))
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
    signer: Option<AccountId>,
    source: Source,
    catalog: Catalog,
) -> Plan {
    Plan {
        snapshots: snapshots.clone(),
        history: HistorySection { dir: cfg.history.dir.clone(), retention: cfg.history.retention },
        log_dir: cfg.node.log_dir.clone(),
        domain,
        signer,
        source,
        catalog,
    }
}
