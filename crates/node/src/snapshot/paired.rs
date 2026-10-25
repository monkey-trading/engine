//! One checkpoint's engine, complete history and byte-authenticated catalog recovery boundary.

use anyhow::{Result, ensure};
use engine_core::types::Millis;
use serde::{Deserialize, Serialize};

use crate::authenticated::{CODEC, Codec};
use crate::config::SnapshotSection;
use crate::log::{CatalogBoundary, Checkpoint, CheckpointDomain, Offset};
use crate::snapshot::head::{self, Envelope, HeadStatement, StoreId};

/// Acknowledgements name the same physical checkpoint, deployment and complete command prefix.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrefixId {
    pub domain: CheckpointDomain,
    pub offset: Offset,
    pub checkpoint: Checkpoint,
    pub command_prefix: [u8; 32],
    pub ts: Millis,
}

/// Every root and codec is required; a later head cannot stand in for this checkpoint's history.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pair {
    pub format: u32,
    pub codec: Codec,
    pub prefix: PrefixId,
    pub history: Envelope,
    pub catalog_store: StoreId,
    pub catalog: CatalogBoundary,
}

impl Pair {
    /// Validates all cross-artifact identities before any database is opened or made writable.
    /// Error cases:
    /// - codec, signer, deployment, physical boundary or exact complete history prefix differs
    pub(crate) fn verify(
        &self,
        cfg: &SnapshotSection,
        expected: &PrefixId,
        retention_ms: Millis,
    ) -> Result<HeadStatement> {
        ensure!(self.format == 1 && self.codec == CODEC, "unsupported paired recovery format");
        ensure!(&self.prefix == expected, "paired recovery prefix differs from engine checkpoint");
        ensure!(self.catalog.checkpoint.offset == expected.offset, "catalog physical checkpoint mismatch");
        ensure!(
            self.catalog.checkpoint.checkpoint == expected.checkpoint,
            "catalog checkpoint differs from engine checkpoint"
        );
        ensure!(self.catalog.prefix.last == expected.offset, "catalog active prefix is not the checkpoint frontier");
        ensure!(
            self.catalog.prefix.info.bytes == self.catalog.checkpoint.end,
            "catalog active bytes extend beyond checkpoint"
        );
        ensure!(
            self.catalog.catalog.root.namespace == self.catalog_store.catalog_namespace(),
            "catalog namespace mismatch"
        );
        ensure!(
            self.catalog.domain.chain_id == expected.domain.chain_id
                && self.catalog.domain.vault == expected.domain.vault.0
                && self.catalog.domain.log_id == self.catalog_store.catalog_namespace(),
            "catalog deployment/generation mismatch"
        );
        let history = head::verify_certificate(cfg, &self.history, expected.domain)?;
        ensure!(history.store != self.catalog_store, "paired history and catalog must have independent generations");
        history.verify_prefix(expected.offset.seq, expected.ts, expected.command_prefix, retention_ms)?;
        Ok(history)
    }
}
