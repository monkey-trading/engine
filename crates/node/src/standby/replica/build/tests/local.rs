//! A replica built from a daily snapshot of a copy that holds none of the segments before the snapshot's.

use super::*;
use crate::history::marks::fold::MarkFold;
use crate::sequencer::replay::carried_tests::facts;

/// A follower whose copy lacks every segment before the one holding its snapshot's checkpoint, as a host that keeps
/// only the log's recent segments holds it, builds its replica from the snapshot, reading nothing before it, to what
/// a whole replay of the full copy holds; a whole replay of that copy is refused, naming the archive.
#[tokio::test]
async fn a_copy_without_the_segments_before_its_snapshot_builds_its_replica_from_it() {
    let (full, snapshots) = (scratch_dir("build-snapshot-local-full"), scratch_dir("build-snapshot-local-daily"));
    write_log(&full).await;
    let seq = publish_middle(&full, &snapshots, &recorded());
    let dir = scratch_dir("build-snapshot-local-copy");
    crate::test_support::copy_dir(&full, &dir);
    assert!(drop_segments_before(&dir, seq) > 1, "the copy held segments before the snapshot's");
    let signed = usable(&snapshots, &dir, &[signer()].into(), u64::MAX).await.expect("a usable snapshot");
    let (_guard, mut alerts) = capture_alerts();
    let from = open_marking(&dir, Some(signed));
    assert_eq!(from.replays(), 0, "the replica replayed nothing whole");
    assert!(alerts.try_recv().is_err(), "the snapshot start pages nothing");
    let refused = try_open(&dir, None, [signer()].into(), None).err().expect("a whole replay of the copy");
    let not_local = refused.chain().find_map(|cause| cause.downcast_ref::<crate::log::NotLocal>());
    assert!(not_local.is_some_and(|not_local| not_local.seq == 0), "{refused:#}");
    let whole = open_marking(&full, None);
    assert_eq!(whole.replays(), 1);
    let (from, whole) = (from.into_log(None, signer(), None).unwrap(), whole.into_log(None, signer(), None).unwrap());
    assert_eq!(facts(&from.replayed), facts(&whole.replayed));
}

// ── Helpers ──

/// The follower of the copy in `dir`, its replica built from `snapshot` or by a whole replay, folding the marks it
/// applies, as a member's does.
fn open_marking(dir: &Path, snapshot: Option<Signed>) -> Follower {
    let marks = MarkFold::new(crate::history::marks::Retentions::defaults());
    let records = (None, Some(std::rc::Rc::new(std::cell::RefCell::new(marks))), None, None);
    let workers = ReplayWorkers::available();
    let opened = Follower::open_pinned(
        dir,
        SEGMENT_COMMANDS,
        workers,
        DOMAIN,
        signer(),
        [signer()].into(),
        records,
        (None, &[]),
        snapshot,
        |_| Ok(()),
    );
    opened.unwrap()
}

/// Removes from the log directory `dir` every segment file before the one holding `seq`, as a host that keeps only
/// the archive's copy of them does; how many it removed.
fn drop_segments_before(dir: &Path, seq: u64) -> usize {
    let mut segments: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|file| file.unwrap().path())
        .filter_map(|path| Some((crate::log::segment_seq(path.file_name()?.to_str()?)?, path)))
        .collect();
    segments.sort_unstable();
    let holding = segments.iter().rposition(|(first, _)| *first <= seq).expect("a segment holds the seq");
    segments[..holding].iter().for_each(|(_, path)| std::fs::remove_file(path).unwrap());
    holding
}
