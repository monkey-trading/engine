//! A takeover's first start under the new signer: the log cut back to the vault's finalized checkpoint.

use std::sync::Arc;

use anyhow::{Context, bail};
use engine_core::types::AccountId;

use crate::config::NodeConfig;
use crate::tee::EnclaveKey;

/// On a takeover's first start under the new signer, cuts the log to the vault's finalized
/// checkpoint. The standby's copy kept the outgoing operator's later published checkpoints past
/// the vault's latest; their signatures are unpostable now, and writable recovery admits only
/// what the vault accepted, so the unposted tail goes before recovery opens the log. A
/// `takeover.json` marker makes the cut resumable: a crash mid-cut is repaired on the next start.
/// Error cases:
/// - a promotion's record names this key, the chain or log cannot be read, or the cut cannot complete
pub(in crate::node) async fn cut_to_finalized_anchor(cfg: &NodeConfig, key: &Arc<EnclaveKey>) -> anyhow::Result<()> {
    use crate::checkpoints::{CheckpointChain, View};
    // A promoted member's copy keeps every command the promotion fixed: it resumes from the copy
    // (`member::promoted`), and no start cuts it back to the anchor.
    if let Some(sealing) = &cfg.tee.sealing
        && crate::node::member::promoted::Record::load(&sealing.dir)?
            .is_some_and(|record| record.key == AccountId(key.address()))
    {
        bail!(
            "a promotion made this key the vault's signer from this copy (promotion.json): it resumes from the \
             copy as a member, never cut back to the vault's anchor"
        );
    }
    let Some(chain) = crate::handover::vault_reader(cfg)? else { return Ok(()) };
    let dir = cfg.node.log_dir.as_path();
    let marker = dir.join("takeover.json");
    let repair = |e: &anyhow::Error| -> anyhow::Error {
        match std::fs::read(&marker) {
            Ok(bytes) => match serde_json::from_slice::<crate::checkpoints::Takeover>(&bytes) {
                Ok(takeover) => {
                    if let Err(cut) = crate::log::truncate_to_checkpoint(dir, takeover.seq, &takeover.root) {
                        return anyhow::anyhow!("{e:#}; and repairing the interrupted cut failed: {cut:#}");
                    }
                    let _ = std::fs::remove_file(&marker);
                    anyhow::anyhow!("{e:#}; repaired the interrupted cut: start again")
                }
                Err(_) => anyhow::anyhow!("{e:#}"),
            },
            Err(_) => anyhow::anyhow!("{e:#}"),
        }
    };
    let state = chain.state(View::Finalized).await.map_err(|e| repair(&e))?;
    if state.signer != AccountId(key.address()) {
        return Ok(());
    }
    let Some((anchor_seq, anchor_root)) = state.checkpoint else { return Ok(()) };
    // Only a tail another key signed is cut: a node's own unposted checkpoints are its poster's
    // queued work, which an ordinary restart must keep.
    let mut last: Option<(u64, String, AccountId)> = None;
    let scan = crate::log::SegmentedLog::open(dir, |entry| {
        if let crate::log::LogEntry::Checkpoint(checkpoint) = &entry {
            last = Some((checkpoint.seq, checkpoint.root.clone(), checkpoint.signer.parse()?));
        }
        Ok(())
    });
    let last = match scan {
        Ok(_) => last,
        Err(e) => {
            let repaired = repair(&e);
            // Retry once after the repair.
            crate::log::SegmentedLog::open(dir, |entry| {
                if let crate::log::LogEntry::Checkpoint(checkpoint) = &entry {
                    last = Some((checkpoint.seq, checkpoint.root.clone(), checkpoint.signer.parse()?));
                }
                Ok(())
            })?;
            if last.is_none() {
                return Err(repaired);
            }
            last
        }
    };
    let last = last.as_ref();
    if last.is_some_and(|(_, _, signer)| *signer == state.signer) {
        return Ok(());
    }
    if last.is_some_and(|(seq, root, _)| *seq > anchor_seq || *root != anchor_root) {
        std::fs::write(
            &marker,
            serde_json::to_string(&crate::checkpoints::Takeover {
                seq: anchor_seq,
                root: anchor_root.clone(),
                signer: state.signer,
            })
            .context("encoding the takeover marker")?,
        )
        .context("writing the takeover marker")?;
        crate::log::truncate_to_checkpoint(dir, anchor_seq, &anchor_root).map_err(|e| repair(&e))?;
        std::fs::remove_file(&marker).context("removing the takeover marker")?;
    }
    Ok(())
}
