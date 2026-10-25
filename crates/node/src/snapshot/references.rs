//! Verified optional-cache references; unsigned filenames and journal IDs are only locators.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};

use crate::config::SnapshotSection;
use crate::log::CheckpointDomain;
use crate::log::catalog_head::{CatalogDomain, CatalogHeadCertificate, HEAD_FORMAT};
use crate::snapshot::generations::VerifiedReferences;
use crate::snapshot::head::StoreId;
use crate::snapshot::paired::PrefixId;
use crate::snapshot::payloads::{Cached, Payloads};
use crate::snapshot::{self, Snapshot, catalog_heads, head};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Kind {
    Pair,
    History,
    Catalog,
    EngineOnly,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct Record {
    pub kind: Kind,
    pub ids: [Option<StoreId>; 2],
}

/// Known publications remain required until this node's own durable pruning removes them.
pub(super) struct Inventory {
    cfg: SnapshotSection,
    domain: CheckpointDomain,
    pub active_catalog: Option<StoreId>,
    payloads: Arc<Payloads>,
    records: BTreeMap<PathBuf, Record>,
    removing: BTreeSet<PathBuf>,
}

impl Inventory {
    /// Starts from a complete authenticated read of the configured cache metadata families.
    /// Error cases:
    /// - incomplete IO or an unsupported potentially authenticated format prevents safe maintenance
    pub fn open(
        cfg: SnapshotSection,
        domain: CheckpointDomain,
        active_catalog: Option<StoreId>,
        payloads: Arc<Payloads>,
    ) -> Result<Self> {
        let records = scan(&cfg, domain, &payloads)?;
        Ok(Self { cfg, domain, active_catalog, payloads, records, removing: BTreeSet::new() })
    }

    /// Runs after Manager's directory barrier and never derives a trusted root from a locator.
    /// Error cases:
    /// - a known required reference disappeared/changed or metadata enumeration is incomplete
    pub fn verify(&mut self, _locators: &[StoreId]) -> Result<VerifiedReferences> {
        let records = scan(&self.cfg, self.domain, &self.payloads)?;
        for (path, previous) in &self.records {
            match records.get(path) {
                Some(actual) => {
                    ensure!(actual == previous, "known cache generation reference changed: {}", path.display())
                }
                None => {
                    ensure!(self.removing.contains(path), "known retained cache is unavailable: {}", path.display())
                }
            }
        }
        self.records = records;
        self.removing.clear();
        Ok(VerifiedReferences::from_verified_certificates(
            self.records.values().flat_map(|record| record.ids).flatten(),
        ))
    }

    /// Returns IDs from an independently verified artifact, never from its filename alone.
    pub fn record(&self, path: &Path) -> Option<Record> {
        self.records.get(path).cloned()
    }
    pub fn referenced(&self, id: StoreId) -> bool {
        self.records.values().any(|record| record.ids.contains(&Some(id)))
    }

    /// Marks only an outgoing artifact whose retirement intent Manager already made durable.
    pub fn removing(&mut self, path: &Path) {
        self.removing.insert(path.to_owned());
    }

    /// Old catalog heads retire before the last outgoing paired cache that names them, so they
    /// cannot keep themselves alive.
    pub fn obsolete_catalog_heads(&self, excluding: Option<&Path>) -> Vec<PathBuf> {
        let mut retained: BTreeSet<_> = self
            .records
            .iter()
            .filter(|(path, record)| record.kind == Kind::Pair && excluding != Some(path.as_path()))
            .filter_map(|(_, record)| record.ids[1])
            .collect();
        retained.extend(self.active_catalog);
        self.records
            .iter()
            .filter(|(_, record)| {
                record.kind == Kind::Catalog && record.ids[0].is_some_and(|id| !retained.contains(&id))
            })
            .map(|(path, _)| path.clone())
            .collect()
    }
}

/// Checks a paired cache's history certificate, catalog boundary, and recovery context, and
/// returns the two generations it keeps alive.
/// Error cases:
/// - the pair is missing, or its certificate, boundary, or context fails verification
fn paired(cfg: &SnapshotSection, domain: CheckpointDomain, snapshot: &Snapshot) -> Result<Record> {
    let pair = snapshot.pair.as_ref().context("signed paired cache is incomplete")?;
    let history = head::verify_certificate(cfg, &pair.history, domain)?;
    let expected = PrefixId {
        domain,
        offset: snapshot.offset,
        checkpoint: snapshot.checkpoint.clone(),
        command_prefix: snapshot.context.command_prefix,
        ts: pair.prefix.ts,
    };
    pair.verify(cfg, &expected, history.head.retention_ms)?;
    snapshot.context.validate(&snapshot.checkpoint, domain)?;
    Ok(Record { kind: Kind::Pair, ids: [Some(history.store), Some(pair.catalog_store)] })
}

/// Reads every candidate in each bounded metadata family, preserving uncertain extra publications.
/// Error cases:
/// - actual IO, an unsupported signed payload or a required authenticated identity is invalid
fn scan(cfg: &SnapshotSection, domain: CheckpointDomain, payloads: &Payloads) -> Result<BTreeMap<PathBuf, Record>> {
    let mut records = BTreeMap::new();
    for (path, verified) in payloads.verified(cfg, domain.chain_id, domain.vault)? {
        let record = match verified? {
            Cached::Payload(snapshot) if snapshot.pair.is_some() => {
                payloads.record(&path, || paired(cfg, domain, &snapshot).map(Some))?
            }
            Cached::Payload(_) | Cached::EngineOnly => Some(Record { kind: Kind::EngineOnly, ids: [None, None] }),
            Cached::Rejected => None,
            Cached::Unsupported(error) => {
                anyhow::bail!("{}: signed snapshot payload is malformed: {error}", path.display())
            }
        };
        if let Some(record) = record {
            records.insert(path, record);
        }
    }
    for path in head::candidates_readonly(cfg)? {
        match head::verified_payload(cfg, &path, domain) {
            Ok(_) => {}
            Err(error) => {
                snapshot::pins::rejected_reference(&path, error)?;
                continue;
            }
        }
        let statement = head::read(cfg, &path, domain)?;
        records.insert(path, Record { kind: Kind::History, ids: [Some(statement.store), None] });
    }
    for (id, path) in catalog_heads::all_candidates(cfg)? {
        let mut bytes = Vec::new();
        File::open(&path)?.take((cfg.max_bytes.get() as u64).saturating_add(1)).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= cfg.max_bytes.get(), "catalog metadata exceeds cache byte bound");
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                snapshot::rejected(&path, &error.into());
                continue;
            }
        };
        let named: Option<[u8; 20]> = value.get("signer").cloned().and_then(|value| serde_json::from_value(value).ok());
        let signature = value
            .get("signature")
            .and_then(|value| value.as_str())
            .and_then(|value| value.parse::<engine_core::eip712::Signature>().ok());
        if named.is_none_or(|signer| !cfg.trusted_signers.iter().any(|trusted| trusted.0 == signer))
            || signature.is_none()
        {
            continue;
        }
        ensure!(
            value.get("body").and_then(|body| body.get("format")).and_then(|format| format.as_u64())
                == Some(u64::from(HEAD_FORMAT)),
            "unsupported potentially authenticated catalog head format"
        );
        let certificate: CatalogHeadCertificate = match serde_json::from_value(value) {
            Ok(certificate) => certificate,
            Err(error) => {
                snapshot::rejected(&path, &error.into());
                continue;
            }
        };
        let trust = catalog_heads::trust(
            cfg,
            CatalogDomain { chain_id: domain.chain_id, vault: domain.vault.0, log_id: id.catalog_namespace() },
        );
        match certificate.verify(&trust) {
            Ok(_) => {}
            Err(error) => {
                snapshot::rejected(&path, &error);
                continue;
            }
        }
        catalog_heads::read_one(cfg, id, &trust, &path)?;
        records.insert(path, Record { kind: Kind::Catalog, ids: [Some(id), None] });
    }
    Ok(records)
}
