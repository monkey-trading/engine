//! Signed history heads, retained-reference pins, and collection of retired versions.

use anyhow::Result;
use tracing::warn;

use crate::history::runtime::{Actor, CERTIFY_COMMANDS, MAX_COLLECTION_TURNS};
use crate::snapshot::head;

impl Actor {
    /// Whether `CERTIFY_COMMANDS` commands have been indexed since the newest signed head.
    pub(super) fn certification_due(&self) -> bool {
        self.current
            .index
            .head()
            .last_seq()
            .is_some_and(|seq| self.certified_seq.is_none_or(|signed| seq.saturating_sub(signed) >= CERTIFY_COMMANDS))
    }

    /// Whether `CERTIFY_COMMANDS` commands have been indexed since collection last reached idle or
    /// stopped progressing; captured checkpoints sign heads but do not collect.
    pub(super) fn collection_due(&self) -> bool {
        self.current
            .index
            .head()
            .last_seq()
            .is_some_and(|seq| self.collected_seq.is_none_or(|done| seq.saturating_sub(done) >= CERTIFY_COMMANDS))
    }

    /// Optional pointer failures preserve live verified history and all uncertain root pins.
    pub(super) fn certify_latest(&mut self) {
        let head = self.current.index.head();
        if head.last_seq().is_none() {
            return;
        }
        if self.last_certified.as_ref() == Some(&head.root) {
            return;
        }
        if let Err(error) = self.settle_pins() {
            warn!(alert = "snapshot_cleanup_failed", "deferring another publication allocation: {error:#}");
            return;
        }
        let root = head.root.clone();
        let certified = (|| {
            {
                let mut publication = self.lifecycle.publication()?;
                publication.settle()?;
                self.pins.refresh(&self.cfg, self.domain)?;
                self.pins.protect(&head.root)?;
                head::prepare(&self.cfg, &mut publication)?;
            }
            // The proof, signature and fsync run outside the lifecycle lock the sequencer takes.
            let staged = head::stage(&self.cfg, self.domain, self.current.id, &self.current.store, head, &self.key)?;
            let mut publication = self.lifecycle.publication()?;
            publication.settle()?;
            staged.publish(&mut publication)
        })();
        match certified {
            Ok(statement) => {
                self.certified_seq = statement.head.last_seq();
                self.last_certified = Some(root);
                self.refresh_pins();
            }
            Err(error) => {
                warn!(alert = "history_head_write_failed", "latest complete pointer remains optional: {error:#}")
            }
        }
    }

    /// Finite authenticated retirement work follows a complete retained-reference refresh.
    /// Error cases:
    /// - a required fresh verified repair or complete-root publication fails
    pub(super) fn maintain(&mut self) -> Result<()> {
        if let Err(error) = self.settle_pins() {
            warn!(alert = "snapshot_cleanup_failed", "deferring version and generation retirement: {error:#}");
            return Ok(());
        }
        if self.current.index.head().root.version.is_none() {
            return Ok(());
        }
        let indexed = self.current.index.head().last_seq();
        let before = self.current.index.head().root;
        for _ in 0..MAX_COLLECTION_TURNS {
            let start = self.current.index.head().root;
            match self.current.index.maintenance(crate::authenticated::COMPLETE_RECEIPT_BUDGET) {
                Ok(report) => {
                    if report.idle || self.current.index.head().root == start {
                        // Done until the next `CERTIFY_COMMANDS`; until then it stays due.
                        self.collected_seq = indexed;
                        break;
                    }
                }
                Err(error) => {
                    self.damaged(&error);
                    return self.repair();
                }
            }
        }
        if self.current.index.head().root != before {
            if let Err(error) = self.publish() {
                self.damaged(&error);
                return self.repair();
            }
            self.certify_latest();
        }
        Ok(())
    }

    /// Releases retired versions only after a complete retained-reference refresh succeeds.
    pub(super) fn refresh_pins(&mut self) {
        if let Err(error) = self.settle_pins() {
            warn!(alert = "snapshot_cleanup_failed", "retaining cache version pins: {error:#}");
        }
    }

    /// Refreshes the complete retained set under the same durability/metadata publication gate.
    /// Error cases:
    /// - a pending directory barrier, reference authentication or version lease cannot complete
    pub(super) fn settle_pins(&mut self) -> Result<()> {
        let mut publication = self.lifecycle.publication()?;
        publication.settle()?;
        let retained = crate::snapshot::pins::catalog_references(&self.cfg, self.domain, &self.lifecycle.payloads())?;
        self.pins.refresh(&self.cfg, self.domain)?;
        self.retained_catalog
            .retain(|offset, view| retained.iter().any(|(at, root)| at == offset && root == &view.certified().root));
        Ok(())
    }
}
