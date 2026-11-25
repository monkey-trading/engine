//! A standby's cold start and takeover over a copy that lacks every segment before a daily snapshot's: each
//! replays the copy from the snapshot, and refuses what a replay of the whole copy refuses after it.

use std::sync::Arc;

use super::refresh::{resign_checkpoints, short_of_last};
use super::*;
use crate::checkpoints::Takeover;
use crate::daily_snapshots::{Listed, Signing};
use crate::handover::{Finalized, copy_start};
use crate::log::{Checkpoint, Offset};
use crate::sequencer::replay::{publish_daily_snapshot, remove_segments_before};
use crate::standby::chain::Chain;
use crate::test_support::copy_dir;

/// A cold start checks a copy whose segments before a daily snapshot's are gone from the snapshot, fetches none of
/// them, pages nothing and marks the copy checked.
#[tokio::test]
async fn a_cold_start_checks_a_copy_from_a_snapshot_without_its_segments_before_it() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let (operator, snapshots, listed) = logged("backfill-snapshot").await;
    let dir = archive_of(&operator, "backfill-snapshot-copy");
    let first = Manifest::load(&dir).unwrap().segments[0].file.clone();
    assert!(remove_segments_before(&dir, listed.seq) > 0, "the copy holds segments before the snapshot's");
    let served = serve(&operator).await;

    start_from(&followed(&served, &served), &dir, &snapshots, finalized(&operator)).await.unwrap();
    assert!(!needed(&dir), "the copy is not marked checked");
    assert!(named(&mut alerts, "alert").is_none(), "the copy was refused");
    assert!(!dir.join(&first).exists(), "the cold start fetched a segment before the snapshot's");
}

/// A cold start refuses a copy whose entries after the snapshot a replay of the whole copy refuses, paging
/// Critical `standby_backfill_refused`, sets it aside and backfills the operator's copy, which it checks from the
/// snapshot.
#[tokio::test]
async fn a_cold_start_refuses_a_copy_the_whole_replay_refuses_after_its_snapshot() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let (operator, snapshots, listed) = logged("backfill-snapshot-refused").await;
    let poisoned = archive_of(&operator, "backfill-snapshot-refused-poisoned");
    let last = Manifest::load(&poisoned).unwrap().segments.last().cloned().unwrap();
    assert!(last.first_seq > listed.seq, "the altered segment is after the snapshot's");
    poison_segment(&poisoned, &last.file);
    let whole = replay_as_takeover(&poisoned, (DOMAIN, Default::default()), signers(), None);
    let said = whole.expect_err("the whole copy replayed").root_cause().to_string();
    let dir = scratch_dir("backfill-snapshot-refused-copy");
    copy_dir(&poisoned, &dir);
    remove_segments_before(&dir, listed.seq);

    let origin = followed(&serve(&operator).await, &serve(&poisoned).await);
    start_from(&origin, &dir, &snapshots, finalized(&operator)).await.unwrap();
    let paged = named(&mut alerts, "alert").expect("the refusal paged nothing");
    assert!(paged["message"].contains(&said) && paged["message"].contains(&last.file), "{paged:?}");
    assert!(aside_of(&dir, "refused").is_dir(), "the refused copy was not set aside");
    assert!(!needed(&dir), "the operator's copy is not marked checked");
    assert_eq!(log_entries(&dir), log_entries(&archive_of(&operator, "backfill-snapshot-refused-honest")));
}

/// A takeover over a copy whose segments before a daily snapshot's are gone refreshes the next segment from the
/// archive, replays the copy from the snapshot, finds its anchor and cuts the copy there.
#[tokio::test]
async fn a_takeover_finalizes_a_copy_from_a_snapshot_without_its_segments_before_it() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (operator, snapshots, listed) = logged("backfill-snapshot-takeover").await;
    let (dir, _, last, bytes) = short_of_last(&operator, "backfill-snapshot-takeover-copy");
    let first = Manifest::load(&dir).unwrap().segments[0].file.clone();
    assert!(last.first_seq > listed.seq, "the refreshed segment is after the snapshot's");
    assert!(remove_segments_before(&dir, listed.seq) > 0, "the copy holds segments before the snapshot's");
    std::fs::write(dir.join(CHECKED), "").unwrap();
    let anchor = last_checkpoint_in(&bytes);
    let takeover = Takeover { seq: anchor.seq, root: anchor.root.clone(), signer: signer() };
    let origins = [serve(&operator).await];

    let copy = (dir.as_path(), Some(snapshots.as_path()));
    crate::standby::finalize(
        &archive_client(false).unwrap(),
        trusted(),
        &origins,
        copy,
        &takeover,
        chain(&dir, &listed),
    )
    .await
    .unwrap_or_else(|e| panic!("the takeover did not finalize: {e:#}"));
    let mut ended = None;
    SegmentedLog::open_from(&dir, Offset::start_of(anchor.seq), |entry| {
        ended = Some(entry);
        Ok(())
    })
    .unwrap();
    assert!(matches!(ended, Some(LogEntry::Checkpoint(held)) if held == anchor), "the copy does not end at the anchor");
    assert!(!needed(&dir), "the copy is not marked checked");
    assert!(!dir.join(&first).exists(), "the takeover fetched a segment before the snapshot's");
    assert!(alerts.try_recv().is_err(), "the takeover paged");
}

/// A takeover whose archive refresh takes a segment whose checkpoints another key signed refuses it from the
/// snapshot, as the whole copy's replay does, drops it, and replays the copy the stream gave from the same
/// snapshot, opened again: here that copy lacks the anchor, so the takeover then fails on that.
#[tokio::test]
async fn a_takeover_refuses_from_a_snapshot_a_refreshed_segment_the_whole_replay_refuses() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (operator, snapshots, listed) = logged("backfill-snapshot-resigned").await;
    let mirror = archive_of(&operator, "backfill-snapshot-resigned-mirror");
    let (dir, before, last, bytes) = short_of_last(&operator, "backfill-snapshot-resigned-copy");
    resign_checkpoints(&mirror, &last.file, &EnclaveKey::generate());
    replay_as_takeover(&mirror, (DOMAIN, Default::default()), signers(), None).expect_err("the whole copy replayed");
    remove_segments_before(&dir, listed.seq);
    std::fs::write(dir.join(CHECKED), "").unwrap();
    let anchor = last_checkpoint_in(&bytes);
    let takeover = Takeover { seq: anchor.seq, root: anchor.root.clone(), signer: signer() };
    let origins = [serve(&mirror).await];

    let copy = (dir.as_path(), Some(snapshots.as_path()));
    let refused = crate::standby::finalize(
        &archive_client(false).unwrap(),
        trusted(),
        &origins,
        copy,
        &takeover,
        chain(&dir, &listed),
    )
    .await
    .expect_err("the takeover took a segment no replay admits");
    assert!(format!("{refused:#}").contains("holds no checkpoint at the vault's latest"), "{refused:#}");
    assert_eq!(std::fs::read_to_string(dir.join(MANIFEST)).unwrap(), before, "the copy's listings");
    assert!(!dir.join(&last.file).exists(), "the refreshed segment is still there");
    assert!(!needed(&dir), "the copy the stream gave is not marked checked");
    assert!(alerts.try_recv().is_err(), "the snapshot was refused");
}

// ── Helpers ──

/// The log of a few ticks in a directory named for `name`, and a directory of snapshots holding one daily snapshot
/// of it at a checkpoint midway through it.
async fn logged(name: &str) -> (PathBuf, PathBuf, Listed) {
    let (operator, snapshots) = (scratch_dir(&format!("{name}-operator")), scratch_dir(&format!("{name}-daily")));
    ticks(&operator, 9).await;
    let signing = Signing { key: Arc::new(EnclaveKey::test_key()), domain: DOMAIN };
    let listed = publish_daily_snapshot(&operator, &snapshots, signing, |held| held / 2);
    (operator, snapshots, listed)
}

/// What `serve` runs at a cold start, under the signers and the daily snapshot in `snapshots` that `copy_start`
/// gives the copy in `dir` once it is fetched, at or before `finalized`.
async fn start_from(followed: &Followed, dir: &Path, snapshots: &Path, finalized: Finalized) -> anyhow::Result<()> {
    let trust = async || {
        let start = copy_start(Some(snapshots), dir, (DOMAIN, &signers()), Some(&finalized)).await;
        Ok(Trust { signers: signers(), start })
    };
    prepare(&archive_client(false).unwrap(), followed, dir, (DOMAIN, Default::default()), trust).await
}

/// The last checkpoint of the log in `operator`, as the vault finalized it.
fn finalized(operator: &Path) -> Finalized {
    let last = log_entries(operator).into_iter().rev().find_map(|entry| match entry {
        LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
        _ => None,
    });
    let last = last.expect("the log holds a checkpoint");
    Finalized { seq: last.seq, root: last.root, signer: last.signer.parse().expect("the checkpoint's signer") }
}

/// The last checkpoint the segment `bytes` holds.
fn last_checkpoint_in(bytes: &[u8]) -> Checkpoint {
    let mut checkpoints = bytes.split(|byte| *byte == b'\n').filter_map(|line| match serde_json::from_slice(line) {
        Ok(LogEntry::Checkpoint(checkpoint)) => Some(checkpoint),
        _ => None,
    });
    checkpoints.next_back().expect("the segment holds a checkpoint")
}

/// The chain of the copy in `dir` as it stands after `listed`'s checkpoint, read from the command at its seq on:
/// the links before that checkpoint, which it restarts, are not needed.
fn chain(dir: &Path, listed: &Listed) -> Chain {
    let mut copied = Chain::new(DOMAIN, signer());
    SegmentedLog::open_from(dir, Offset::start_of(listed.seq), |entry| copied.record(entry)).unwrap();
    copied
}

/// The takeover's trust: the test sequencer's enclave, and the threads that check seals ahead.
fn trusted() -> (CheckpointDomain, BTreeSet<AccountId>, ReplayWorkers) {
    (DOMAIN, signers(), Default::default())
}

/// The test sequencer's enclave, which seals its commands and signs its checkpoints and snapshots.
fn signer() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}
