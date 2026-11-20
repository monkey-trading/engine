//! A takeover's first start under the new signer: the log cut back to the vault's finalized checkpoint.

use std::path::Path;
use std::sync::Arc;

use anyhow::bail;
use engine_core::types::AccountId;

use crate::checkpoints::Takeover;
use crate::config::NodeConfig;
use crate::log::{Checkpoint, LogEntry, SegmentedLog};
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
    let Some(anchor) = state.checkpoint else { return Ok(()) };
    if let Some(to) = cut_due(dir, state.signer, anchor)? {
        // Behind the marker every open of a copy finishes from, published and removed for good.
        crate::standby::cut_marked(dir, &to)?;
    }
    Ok(())
}

/// The cut back to the vault's finalized checkpoint `(seq, root)` that the log in `dir` is due under
/// `signer`, the vault's: one when the log's last checkpoint is another key's and past it or another
/// at its seq. Only a tail another key signed is cut: a node's own unposted checkpoints are its
/// poster's queued work, which an ordinary restart keeps. An ordinary restart finds its own key's in
/// the log's newest segments, and so reads no more of a long log than of a short one; a cut writes
/// the log, so it is decided on a whole read, which refuses a log that does not open before anything
/// is cut.
/// Error cases:
/// - the log cannot be read, or a checkpoint's signer is not an address
fn cut_due(dir: &Path, signer: AccountId, (seq, root): (u64, String)) -> anyhow::Result<Option<Takeover>> {
    let signer_of = |checkpoint: &Checkpoint| checkpoint.signer.parse::<AccountId>();
    if crate::log::last_checkpoint(dir)?.as_ref().map(signer_of).transpose()? == Some(signer) {
        return Ok(None);
    }
    let mut last: Option<Checkpoint> = None;
    SegmentedLog::open(dir, |entry| {
        if let LogEntry::Checkpoint(checkpoint) = entry {
            last = Some(checkpoint);
        }
        Ok(())
    })?;
    let Some(last) = last else { return Ok(None) };
    if signer_of(&last)? == signer || (last.seq <= seq && last.root == root) {
        return Ok(None);
    }
    Ok(Some(Takeover { seq, root, signer }))
}

#[cfg(test)]
mod tests {
    use engine_core::command::Command;

    use engine_core::command::Sequenced;

    use super::*;
    use crate::log::{CommandEntry, Manifest};
    use crate::test_support::{TestSequencer, dev_config, log_entries, scratch_dir};

    const OURS: AccountId = AccountId([0x11; 20]);
    const OUTGOING: AccountId = AccountId([0x22; 20]);

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

    /// An ordinary restart, whose last checkpoint is its own key's, reads the log's newest segments
    /// alone: a segment before them that no longer reads, which a whole read refuses, cuts nothing.
    #[test]
    fn an_ordinary_restart_is_decided_from_the_newest_segments() {
        let dir = log("takeover-ours", &[(1, OUTGOING, 1), (3, OURS, 3)]);
        damage_first_segment(&dir);
        assert!(SegmentedLog::open(&dir, |_| Ok(())).is_err(), "a whole read refuses the first segment");
        assert_eq!(cut_due(&dir, OURS, (1, root(1))).unwrap(), None);
    }

    /// A tail another key signed past the vault's checkpoint is cut back to it, a log that holds the
    /// vault's checkpoint last is not, and a cut is decided on a whole read, which refuses a log a
    /// segment of which no longer reads before anything is cut.
    #[test]
    fn a_cut_is_due_only_for_another_keys_tail_and_decided_on_a_whole_read() {
        let dir = log("takeover-due", &[(1, OUTGOING, 1), (3, OUTGOING, 3)]);
        let due = cut_due(&dir, OURS, (1, root(1))).unwrap();
        assert_eq!(due.map(|to| (to.seq, to.root, to.signer)), Some((1, root(1), OURS)));
        assert_eq!(cut_due(&dir, OURS, (3, root(3))).unwrap(), None, "the log ends at the vault's checkpoint");
        damage_first_segment(&dir);
        cut_due(&dir, OURS, (1, root(1))).expect_err("a log that does not open is refused, so nothing is cut");
    }

    // ── Helpers ──

    /// A log in a directory named for `label` of a command at each seq to the last checkpoint's, in
    /// segments of two commands, with a checkpoint `(seq, signer, root)` after the command at each seq.
    fn log(label: &str, checkpoints: &[(u64, AccountId, u8)]) -> std::path::PathBuf {
        let dir = scratch_dir(label);
        let manifest = tokio::sync::watch::channel(Manifest::default()).0;
        let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, manifest).unwrap();
        let last = checkpoints.last().map_or(0, |(seq, _, _)| *seq);
        for seq in 0..=last {
            let sequenced = Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick };
            writer
                .append(&LogEntry::Command(CommandEntry { sequenced, events: Some(Vec::new()), seal: None }))
                .unwrap();
            for (_, signer, byte) in checkpoints.iter().filter(|(at, _, _)| *at == seq) {
                writer.append(&LogEntry::Checkpoint(checkpoint(seq, *signer, *byte))).unwrap();
            }
        }
        writer.flush().unwrap();
        dir
    }

    /// A checkpoint at `seq` that `signer` names, with `root(byte)`.
    fn checkpoint(seq: u64, signer: AccountId, byte: u8) -> Checkpoint {
        Checkpoint {
            parent: None,
            seq,
            root: root(byte),
            signature: format!("0x{}", "00".repeat(65)),
            signer: signer.to_string(),
            domain: None,
            deferred_withdrawals: false,
        }
    }

    /// The root every byte of which is `byte`.
    fn root(byte: u8) -> String {
        format!("0x{}", hex::encode([byte; 32]))
    }

    /// Rewrites the log's first segment, which its listing then no longer matches.
    fn damage_first_segment(dir: &Path) {
        let first = dir.join("00000000000000000000.jsonl");
        let mut bytes = std::fs::read(&first).unwrap();
        bytes.extend_from_slice(b"\n");
        std::fs::write(first, bytes).unwrap();
    }
}
