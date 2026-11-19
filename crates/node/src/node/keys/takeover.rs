//! A takeover's first start under the new signer: the log cut back to the vault's finalized checkpoint.

use std::sync::Arc;

use anyhow::bail;
use engine_core::types::AccountId;

use crate::config::NodeConfig;
use crate::tee::EnclaveKey;

/// On a takeover's first start under the new signer, cuts the log to the vault's finalized
/// checkpoint. The standby's copy kept the outgoing operator's later published checkpoints past
/// the vault's latest; their signatures are unpostable now, and writable recovery admits only
/// what the vault accepted, so the unposted tail goes before recovery opens the log. The cut runs
/// behind a `takeover.json` marker (`standby::cut_marked`), and a cut a crash interrupted is finished at
/// the next start before anything reads the log.
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
    let dir = cfg.node.log_dir.as_path();
    // A cut a crash interrupted is finished before anything reads the log, whether or not what it left
    // opens: the marker every open of a copy acts on never outlives its cut to cut a grown log again.
    crate::standby::finish_cut(dir)?;
    let Some(chain) = crate::handover::vault_reader(cfg)? else { return Ok(()) };
    let state = chain.state(View::Finalized).await?;
    if state.signer != AccountId(key.address()) {
        return Ok(());
    }
    let Some((anchor_seq, anchor_root)) = state.checkpoint else { return Ok(()) };
    // Only a tail another key signed is cut: a node's own unposted checkpoints are its poster's
    // queued work, which an ordinary restart must keep.
    let mut last: Option<(u64, String, AccountId)> = None;
    crate::log::SegmentedLog::open(dir, |entry| {
        if let crate::log::LogEntry::Checkpoint(checkpoint) = &entry {
            last = Some((checkpoint.seq, checkpoint.root.clone(), checkpoint.signer.parse()?));
        }
        Ok(())
    })?;
    let last = last.as_ref();
    if last.is_some_and(|(_, _, signer)| *signer == state.signer) {
        return Ok(());
    }
    if last.is_some_and(|(seq, root, _)| *seq > anchor_seq || *root != anchor_root) {
        // Behind the marker every open of a copy finishes from, published and removed for good.
        let to = crate::checkpoints::Takeover { seq: anchor_seq, root: anchor_root.clone(), signer: state.signer };
        crate::standby::cut_marked(dir, &to)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use engine_core::command::Command;

    use super::*;
    use crate::log::LogEntry;
    use crate::test_support::{TestSequencer, dev_config, log_entries, scratch_dir};

    /// A takeover start finishes a cut its marker names before it reads the log, even when what the crash
    /// left opens, and removes the marker: none is left to cut the log again once it grows.
    #[tokio::test]
    async fn a_takeover_start_finishes_a_marked_cut_before_it_reads_the_log() {
        let dir = scratch_dir("takeover-start-marker");
        let node = TestSequencer::start(&dir, 1024);
        for _ in 0..6 {
            node.submit(Command::Tick).await;
        }
        node.stop().await;
        let first = log_entries(&dir)
            .into_iter()
            .find_map(|entry| match entry {
                LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
                _ => None,
            })
            .expect("the log holds a checkpoint");
        let key = Arc::new(EnclaveKey::test_key());
        let marker =
            crate::checkpoints::Takeover { seq: first.seq, root: first.root.clone(), signer: AccountId(key.address()) };
        std::fs::write(dir.join("takeover.json"), serde_json::to_vec(&marker).unwrap()).unwrap();
        let mut cfg = dev_config();
        (cfg.node.log_dir, cfg.tee.sealing) = (dir.clone(), None);
        // Whatever the chain answers, the cut is finished first.
        let _ = cut_to_finalized_anchor(&cfg, &key).await;
        assert!(!dir.join("takeover.json").exists(), "the marker outlived its cut");
        assert!(
            matches!(log_entries(&dir).last(), Some(LogEntry::Checkpoint(checkpoint)) if checkpoint.seq == first.seq),
            "the cut was not finished"
        );
    }
}
