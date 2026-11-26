//! A handover successor whose copy lacks every segment before a daily snapshot's: the restore before it connects
//! (`sync_predecessor_log`) and the take (`take_with`) replay the copy from the snapshot, with no whole replay,
//! and refuse what a replay of the whole copy refuses after it.

use super::copies::{answer_stop, archive, incumbent, mirror_of, mirrored, signers, ticks};
use super::*;
use crate::config::DailySnapshotsSection;
use crate::daily_snapshots::{Listed, Signing};
use crate::handover::{Finalized, sync_predecessor_log};
use crate::log::{Manifest, Offset, SegmentedLog};
use crate::sequencer::replay::{publish_daily_snapshot, remove_segments_before};
use crate::test_support::{TestSequencer, copy_dir, rewrite_command};

/// A successor whose copy of the mirror's archive lacks the segments before a daily snapshot's, which the vault
/// finalized, restores it and takes the keys and the rest of the log, each replaying the copy from the snapshot
/// and none the whole copy; its log ends at the predecessor's head.
#[tokio::test]
async fn a_restore_and_a_take_replay_a_copy_from_a_snapshot_without_its_segments_before_it() {
    let operator = scratch_dir("snapshot-handover-operator");
    let node = TestSequencer::start(&operator, 1024);
    ticks(&node, 12).await;
    let mut exchange = Exchange::start_with("snapshot-handover", Some(node.tx.clone()), Some(operator.clone())).await;
    exchange.make_upgrade_current();
    let mirror = mirror_of(&operator, "snapshot-handover-mirror", |_, _| {});
    let (listed, finalized) = snapshot_of(&mut exchange, &mirror, "snapshot-handover").await;
    let log = exchange.successor_log.clone();
    let first = Manifest::load(&log).unwrap().segments[0].file.clone();

    sync_predecessor_log(&exchange.successor, &incumbent(), (signers(&exchange), Some(&finalized)), &exchange.metrics)
        .await
        .unwrap_or_else(|e| panic!("the restore refused the copy: {e:#}"));
    assert_eq!(exchange.metrics.full_replays.get(), 0, "the restore replayed the whole copy");
    let stopped = answer_stop(&mut exchange, node, operator.clone());
    exchange.take().await.unwrap_or_else(|e| panic!("the successor did not take the keys: {e:#}"));
    let head = stopped.await.unwrap();
    assert_eq!(exchange.metrics.full_replays.get(), 0, "the take replayed the whole copy");
    assert!(!log.join(&first).exists(), "the successor fetched a segment before the snapshot's");
    let mut last = None;
    SegmentedLog::open_from(&log, Offset::start_of(listed.seq), |entry| {
        last = Some(entry);
        Ok(())
    })
    .unwrap();
    assert!(matches!(last, Some(LogEntry::Checkpoint(held)) if held.seq == head), "the log does not end at the head");
}

/// A successor whose copy lacks the segments before a daily snapshot's, and whose entries after it a replay of the
/// whole copy refuses, is refused by the restore from the snapshot, for the same entry, before it asks for the
/// keys, and the predecessor sequences on.
#[tokio::test]
async fn a_restore_refuses_from_a_snapshot_a_copy_the_whole_replay_refuses_after_it() {
    let operator = scratch_dir("snapshot-refused-operator");
    let node = TestSequencer::start(&operator, 1024);
    ticks(&node, 12).await;
    let mut exchange = Exchange::start_with("snapshot-refused", Some(node.tx.clone()), Some(operator.clone())).await;
    exchange.make_upgrade_current();
    let mirror = mirror_of(&operator, "snapshot-refused-mirror", |_, _| {});
    let (listed, finalized) = snapshot_of(&mut exchange, &mirror, "snapshot-refused").await;
    // The mirror and the successor's copy alter the last command they hold, after the snapshot's checkpoint.
    let last = Manifest::load(&mirror).unwrap().segments.last().cloned().unwrap();
    assert!(listed.seq < last.last_seq, "the altered command is after the snapshot");
    for dir in [&mirror, &exchange.successor_log] {
        rewrite_command(dir, last.last_seq, |command| {
            let rejected = engine_core::event::Event::AdminRejected { reason: "poisoned".into() };
            command.events.get_or_insert_with(Vec::new).push(rejected);
        });
    }
    let whole =
        crate::sequencer::replay::replay_copy(&mirror, exchange.domain(), signers(&exchange), Default::default());
    let said = whole.err().expect("the whole copy replayed").root_cause().to_string();

    let refused = sync_predecessor_log(
        &exchange.successor,
        &incumbent(),
        (signers(&exchange), Some(&finalized)),
        &exchange.metrics,
    )
    .await
    .expect_err("the successor restored a copy the whole replay refuses");
    assert!(format!("{refused:#}").contains(&said) && format!("{refused:#}").contains(&last.file), "{refused:#}");
    assert!(exchange.drains.try_recv().is_err(), "the predecessor was asked to stop");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");
    ticks(&node, 1).await;
    node.stop().await;
}

// ── Helpers ──

/// Publishes a daily snapshot of the archive in `mirror`, midway through it, into a directory named for `name`
/// that the successor's `[daily_snapshots]` names; serves `mirror` as the successor's only archive mirror; has the
/// successor's copy hold the mirror's without the segments before the snapshot's, as a host that keeps only those;
/// and has the vault finalize the snapshot's checkpoint. The snapshot's listing, and that checkpoint.
async fn snapshot_of(exchange: &mut Exchange, mirror: &std::path::Path, name: &str) -> (Listed, Finalized) {
    let snapshots = scratch_dir(&format!("{name}-daily"));
    let signing = Signing { key: Arc::new(crate::tee::EnclaveKey::test_key()), domain: exchange.domain() };
    let listed = publish_daily_snapshot(mirror, &snapshots, signing, |held| held / 2);
    exchange.successor.daily_snapshots = Some(DailySnapshotsSection::at(snapshots));
    exchange.successor.tee.sealing = Some(mirrored(exchange, &archive(mirror).await));
    copy_dir(mirror, &exchange.successor_log);
    assert!(remove_segments_before(&exchange.successor_log, listed.seq) > 0, "the copy holds earlier segments");
    *exchange.finalized.lock().unwrap() = Some((listed.seq, listed.root.clone()));
    let signer = engine_core::types::AccountId(crate::tee::EnclaveKey::test_key().address());
    let finalized = Finalized { seq: listed.seq, root: listed.root.clone(), signer, accepted: Default::default() };
    (listed, finalized)
}
