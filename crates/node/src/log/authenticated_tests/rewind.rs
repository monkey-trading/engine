//! Resuming the catalog from a cache's boundary when the log lost a tail a newer head covered.

use super::*;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

use crate::log::authenticated_tests::head_support::latest;

/// The tip lists segments the manifest lost. Recovery resumes from the boundary with every segment
/// the log still lists, those past the live window waiting for the post-preflight fold.
#[test]
fn a_tip_past_the_manifest_resumes_the_catalog_from_the_boundary() {
    let mut f = fixture("catalog-rewind-lost-segments", 1);
    for seq in 0..3 {
        f.writer.append(&command(seq)).expect("initial");
    }
    let c = boundary(&mut f, 2);
    for seq in 3..73 {
        f.writer.append(&command(seq)).expect("kept");
    }
    f.writer.sync().expect("kept bytes durable");
    let kept = log_files(&f.dir);
    for seq in 73..80 {
        f.writer.append(&command(seq)).expect("lost");
    }
    f.writer.sync().expect("lost bytes durable");
    f.writer.flush_catalog().expect("the tip lists the lost segments");
    let t = latest(&f.heads);
    drop(f.writer);
    restore(&f.dir, &kept);
    let log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, Some(&t), |_| Ok(())).expect("recovery at C");
    assert!(log.catalog_rewound());
    let view = log.manifest_view();
    let catalog = view.catalog.as_ref().expect("recovered catalog");
    assert_eq!(catalog.certified(), &c.catalog, "the catalog resumes from the boundary");
    assert_eq!(catalog.len() + log.pending_catalog.len() as u64, 72, "every segment the manifest still lists");
    assert!(view.authority.is_none(), "segments past the live window wait for the post-preflight fold");
}

/// A segment file the tip covers past the recovered active segment that is still on disk was
/// never lost: a certified completion the manifest no longer lists, or the segment the tip
/// holds open. Recovery rejects the cache so it falls back to full replay's directory-wide
/// refusal, instead of rewinding and wedging the writer's first rotation on the leftover file.
/// A certified segment at the active segment's own offset is not past it: recovery resumes
/// from it.
#[test]
fn a_tip_segment_still_on_disk_past_the_recovered_log_rejects_the_cache() {
    for kept in [72, 73, 74, 75, 79] {
        let mut f = fixture(&format!("catalog-rewind-orphan-{kept}"), 1);
        for seq in 0..3 {
            f.writer.append(&command(seq)).expect("initial");
        }
        let c = boundary(&mut f, 2);
        for seq in 3..73 {
            f.writer.append(&command(seq)).expect("kept");
        }
        f.writer.sync().expect("kept bytes durable");
        let kept_files = log_files(&f.dir);
        for seq in 73..80 {
            f.writer.append(&command(seq)).expect("lost");
        }
        f.writer.sync().expect("lost bytes durable");
        f.writer.flush_catalog().expect("the tip lists the lost segments");
        // Keep the bytes of one segment the tip certifies past the synced manifest, as storage
        // that lost the synced manifest append but kept the segment files would.
        let orphan = f.dir.join(format!("{kept:020}.jsonl"));
        let orphan_bytes = fs::read(&orphan).expect("orphan segment");
        let t = latest(&f.heads);
        drop(f.writer);
        restore(&f.dir, &kept_files);
        fs::write(orphan, orphan_bytes).expect("orphan segment back");
        let result = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, Some(&t), |_| Ok(()));
        if kept == 72 {
            let log = result.unwrap_or_else(|error| panic!("kept=72 recovery failed: {error:#}"));
            assert!(log.catalog_rewound());
        } else {
            let error = result.err().unwrap_or_else(|| panic!("kept={kept} unexpectedly recovered"));
            assert!(format!("{error:#}").contains("still on disk"), "{error:#}");
        }
    }
}

/// The tip can name nothing past the recovered log — it sealed before the lost append, and the
/// recovered log lists every segment it covers — while storage that kept the file the recovered
/// log's next command would rotate into, but lost the manifest's listing of the segment before
/// it, still holds bytes the rewind would declare lost. Recovery rejects the cache so it falls
/// back to full replay's directory-wide refusal, instead of rewinding and wedging the writer's
/// first rotation into the leftover file.
#[test]
fn a_next_command_segment_still_on_disk_rejects_the_history_lost_rewind() {
    let mut f = fixture("catalog-rewind-next-leftover", 1);
    for seq in 0..3 {
        f.writer.append(&command(seq)).expect("initial");
    }
    let c = boundary(&mut f, 2);
    for seq in 3..9 {
        f.writer.append(&command(seq)).expect("before the tip");
    }
    f.writer.flush_catalog().expect("the tip seals before the lost append");
    let t = latest(&f.heads);
    for seq in 9..11 {
        f.writer.append(&command(seq)).expect("after the tip");
    }
    f.writer.sync().expect("durable");
    drop(f.writer);
    // Storage loses only the manifest's last line, the listing of segment 9, keeping its file
    // and the file the next command starts.
    let path = f.dir.join(MANIFEST);
    let manifest = fs::read_to_string(&path).expect("manifest");
    let cut = manifest.trim_end_matches('\n').rfind('\n').map_or(0, |i| i + 1);
    fs::write(&path, &manifest[..cut]).expect("manifest loses its last line");
    let mut log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, Some(&t), |_| Ok(())).expect("recovery");
    assert!(!log.catalog_rewound(), "the recovered log lists every segment the tip covers");
    let error = log.rewind_catalog(f.read.clone(), &c).expect_err("the leftover next segment refuses the rewind");
    assert!(format!("{error:#}").contains("still on disk"), "{error:#}");
}

/// The recovered log can end exactly at a segment boundary with the next segment's file
/// already created but still empty, as a crash between the rotation that linked it and the new
/// segment's first sync leaves it. That file is the active segment the writer appends to, not a
/// leftover past the recovered log: the history-lost rewind keeps it.
#[test]
fn an_empty_active_segment_named_for_the_next_seq_is_not_a_leftover() {
    let mut f = fixture("catalog-rewind-empty-active", 1);
    for seq in 0..3 {
        f.writer.append(&command(seq)).expect("initial");
    }
    let c = boundary(&mut f, 2);
    for seq in 3..10 {
        f.writer.append(&command(seq)).expect("to a segment boundary");
    }
    f.writer.sync().expect("durable");
    // Appending command 10 lists segment 9 and links segment 10; a crash before the new
    // segment's first sync loses the buffered append and leaves the file empty, which forgetting
    // the unflushed writer reproduces (a drop would flush the buffer).
    f.writer.append(&command(10)).expect("into the new segment");
    assert_eq!(fs::read(f.dir.join("00000000000000000010.jsonl")).expect("new segment"), Vec::<u8>::new());
    std::mem::forget(f.writer);
    let mut log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, None, |_| Ok(())).expect("recovery");
    assert!(!log.catalog_rewound());
    log.rewind_catalog(f.read.clone(), &c).expect("the empty active segment is not a leftover");
    assert!(log.catalog_rewound());
}

/// A history head can show a lost tail the tip covers past the log's end. The rewind takes the
/// segments only the tip listed from their files again, accepting only the bytes it replayed.
#[test]
fn a_rewind_rereads_only_segments_matching_the_tip() {
    for altered in [false, true] {
        let mut f = fixture(&format!("catalog-rewind-reread-{altered}"), 1);
        for seq in 0..3 {
            f.writer.append(&command(seq)).expect("initial");
        }
        let c = boundary(&mut f, 2);
        for seq in 3..10 {
            f.writer.append(&command(seq)).expect("suffix");
        }
        f.writer.sync().expect("durable");
        f.writer.flush_catalog().expect("the tip lists the suffix segments");
        let t = latest(&f.heads);
        drop(f.writer);
        let mut log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, Some(&t), |_| Ok(())).expect("recovery");
        assert!(!log.catalog_rewound(), "the log holds every segment the tip lists");
        if altered {
            let path = f.dir.join(format!("{:020}.jsonl", 4));
            let text = fs::read_to_string(&path).expect("segment");
            fs::write(&path, text.replacen("\"ts\":4,", "\"ts\":5,", 1)).expect("host changes a replayed segment");
        }
        let result = log.rewind_catalog(f.read.clone(), &c);
        assert_eq!(result.is_ok(), !altered, "{result:?}");
        if !altered {
            let catalog = log.manifest_view().catalog.expect("rewound catalog");
            assert_eq!(catalog.certified(), &c.catalog);
            assert_eq!(catalog.len(), t.catalog().completed_segments);
        }
    }
}

/// A rewind that rebuilds on the boundary alone (no continuation head) reuses the segments the
/// manifest lists, so their files are not leftover: the history-lost rewind refuses nothing
/// here and keeps every listed segment.
#[test]
fn a_boundary_rewind_keeps_the_listed_segments() {
    let mut f = fixture("catalog-rewind-boundary-only", 1);
    for seq in 0..3 {
        f.writer.append(&command(seq)).expect("initial");
    }
    let c = boundary(&mut f, 2);
    for seq in 3..80 {
        f.writer.append(&command(seq)).expect("suffix");
    }
    f.writer.sync().expect("durable");
    drop(f.writer);
    let mut log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, None, |_| Ok(())).expect("recovery");
    assert!(!log.catalog_rewound());
    log.rewind_catalog(f.read.clone(), &c).expect("the boundary rewind keeps the listed segments");
    assert!(log.catalog_rewound());
    let view = log.manifest_view();
    let catalog = view.catalog.as_ref().expect("recovered catalog");
    assert_eq!(catalog.len() + log.pending_catalog.len() as u64, 79, "every listed segment");
}

/// The tip behind a retained suffix: the newest surviving head sealed early, the log then took
/// more than a live window of segments, and a signed history head shows a lost tail. The
/// history-lost rewind reuses every listed segment; the tip's signed open position names a
/// segment the log completed long ago, so its file must not read as leftover.
#[test]
fn a_rewind_with_the_tip_behind_a_long_suffix_keeps_the_listed_segments() {
    let mut f = fixture("catalog-rewind-tip-behind-suffix", 1);
    for seq in 0..3 {
        f.writer.append(&command(seq)).expect("initial");
    }
    let c = boundary(&mut f, 2);
    for seq in 3..10 {
        f.writer.append(&command(seq)).expect("before the tip");
    }
    f.writer.flush_catalog().expect("the tip seals early");
    let t = latest(&f.heads);
    for seq in 10..80 {
        f.writer.append(&command(seq)).expect("past the tip");
    }
    f.writer.sync().expect("durable");
    drop(f.writer);
    let mut log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, Some(&t), |_| Ok(())).expect("recovery");
    assert!(!log.catalog_rewound(), "the log lists every segment the tip covers");
    log.rewind_catalog(f.read.clone(), &c).expect("the rewind keeps the segments past the tip");
    assert!(log.catalog_rewound());
    let view = log.manifest_view();
    let catalog = view.catalog.as_ref().expect("recovered catalog");
    assert_eq!(catalog.len() + log.pending_catalog.len() as u64, 79, "every listed segment");
}

/// The tip sealed a checkpoint the log lost, and the log then took another checkpoint at its
/// offset and completed that segment. The catalog would refuse that segment, so the cache is
/// rejected.
#[test]
fn a_completed_segment_replacing_a_checkpoint_the_tip_holds_rejects_the_cache() {
    let mut f = fixture("catalog-replaced-checkpoint", 8);
    let mut other = fixture("catalog-replaced-checkpoint-log", 8);
    for seq in 0..3 {
        f.writer.append(&command(seq)).expect("initial");
        other.writer.append(&command(seq)).expect("same initial");
    }
    let c = boundary(&mut f, 2);
    other.writer.append(&checkpoint(2)).expect("same checkpoint");
    for seq in 3..6 {
        f.writer.append(&command(seq)).expect("suffix");
        other.writer.append(&command(seq)).expect("same suffix");
    }
    boundary(&mut f, 5);
    let LogEntry::Checkpoint(mut replaced) = checkpoint(5) else { unreachable!() };
    replaced.root = "ff".repeat(32);
    other.writer.append(&LogEntry::Checkpoint(replaced)).expect("another checkpoint at the same offset");
    for seq in 6..9 {
        other.writer.append(&command(seq)).expect("completes the segment");
    }
    other.writer.sync().expect("durable");
    let t = latest(&f.heads);
    drop(f.writer);
    restore(&f.dir, &log_files(&other.dir));
    let error = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, Some(&t), |_| Ok(())).err().expect("rejected");
    assert!(format!("{error:#}").contains("checkpoint differs"), "{error:#}");
}

// ── Helpers ──

/// The manifest and segment files in `dir`, by name.
fn log_files(dir: &Path) -> BTreeMap<OsString, Vec<u8>> {
    fs::read_dir(dir)
        .expect("log directory")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .map(|path| (path.file_name().expect("name").to_owned(), fs::read(&path).expect("log file")))
        .collect()
}

/// Puts the log in `dir` back to `kept`, as a host truncating it or storage losing writes would.
fn restore(dir: &Path, kept: &BTreeMap<OsString, Vec<u8>>) {
    for name in log_files(dir).into_keys().filter(|name| !kept.contains_key(name)) {
        fs::remove_file(dir.join(name)).expect("remove lost file");
    }
    for (name, bytes) in kept {
        fs::write(dir.join(name), bytes).expect("restore kept file");
    }
}
