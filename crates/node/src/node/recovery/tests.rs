//! Which starts count a full replay in `engine_full_replays`: the log on disk replayed in full, with
//! no `[snapshots]` or with one that falls back to the whole log, counted once it has run.

use super::*;
use crate::config::SnapshotSection;
use crate::test_support::{dev_config, sample_around, scratch_dir};

/// A start that replays the log on disk counts one full replay, with `[snapshots]` too when no
/// snapshot signer is pinned and recovery falls back to the whole log.
#[tokio::test]
async fn a_start_counts_its_full_replay_with_or_without_snapshots() {
    for snapshots in [false, true] {
        let root = scratch_dir(&format!("recovery-replays-{snapshots}"));
        let mut cfg = dev_config();
        cfg.node.log_dir = root.join("log");
        cfg.history.dir = root.join("history");
        cfg.snapshots = snapshots.then(|| SnapshotSection {
            dir: root.join("snapshots"),
            retain: 4.try_into().unwrap(),
            max_bytes: (16 * 1024 * 1024).try_into().unwrap(),
            trusted_signers: Vec::new(),
        });
        let key = Arc::new(EnclaveKey::test_key());
        write_log(&cfg, &key).await;
        let marks = marks::startup(&cfg, key.marks_key(), false).unwrap();
        let metrics = Metrics::new();
        let recovered = recover(&cfg, &key, None, &marks, None, &metrics).await.unwrap();
        assert_eq!(recovered.log.snapshot_sequence(), None, "snapshots {snapshots}: a snapshot recovered the log");
        assert_eq!(metrics.full_replays.get(), 1, "snapshots {snapshots}: the full replay is not counted once");
    }
}

/// Writes the log `cfg` names under its genesis, with a few ticks.
async fn write_log(cfg: &NodeConfig, key: &Arc<EnclaveKey>) {
    let (manifest, _) = tokio::sync::watch::channel(crate::log::Manifest::default());
    let seq_cfg = crate::sequencer::SequencerConfig {
        genesis: crate::sequencer::genesis_commands(&cfg.genesis),
        ..crate::test_support::sequencer_config(&cfg.node.log_dir, manifest)
    };
    let (stream, withdrawals) = (tokio::sync::broadcast::channel(16).0, tokio::sync::mpsc::unbounded_channel().0);
    let mut seq = Sequencer::recover(seq_cfg, key.clone(), stream, withdrawals).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&cfg.genesis).unwrap();
    seq.trust(sample_around(&crate::clock::SystemClock)).unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));
    for _ in 0..4 {
        let (reply, applied) = tokio::sync::oneshot::channel();
        let cmd = engine_core::command::Command::Tick;
        tx.send(crate::sequencer::Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        applied.await.unwrap();
    }
    drop(tx);
    task.await.unwrap().unwrap();
}
