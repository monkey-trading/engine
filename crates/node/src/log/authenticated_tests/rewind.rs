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
