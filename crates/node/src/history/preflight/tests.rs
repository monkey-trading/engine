//! Read committed WAL data without creating, updating, or deleting original SQLite sidecars.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine_core::Engine;
use engine_core::command::{Command, Sequenced};

use crate::config::HistorySection;
use crate::history::INDEX_FILE;
use crate::history::preflight::HistoryPlan;
use crate::history::store::Writer;
use crate::log::{CommandEntry, LogEntry, Offset};
use crate::sequencer::{StreamBody, StreamMsg};
use crate::test_support::{capture_alerts, scratch_dir};

#[test]
fn valid_history_resumes_without_alert_or_byte_changes_for_absent_empty_and_committed_wal() {
    for (wal_kind, with_shm) in [("none", false), ("empty", false), ("frames", false), ("frames", true)] {
        let with_wal = wal_kind == "frames";
        let root = scratch_dir(&format!("history-private-wal-{wal_kind}-{with_shm}"));
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let cfg = HistorySection { dir: target.clone(), retention: Duration::from_secs(60) };
        let (writer, _) = Writer::open(&source.join(INDEX_FILE), cfg.retention_ms().unwrap()).unwrap();
        let mut writer = Some(writer);
        let mut engine = Engine::new();
        let mut entries = Vec::new();
        for seq in 0..2 {
            let sequenced = Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick };
            let events = engine.apply(&sequenced).unwrap();
            let entry = CommandEntry { seal: None, sequenced, events: Some(events) };
            writer
                .as_mut()
                .unwrap()
                .write(&[Arc::new(StreamMsg {
                    offset: Offset::start_of(seq),
                    body: StreamBody::Command(entry.clone()),
                })])
                .unwrap();
            entries.push(entry);
        }
        // Copy the idle writer's committed files before its close can checkpoint/delete WAL.
        // This produces a genuine recovery image whose newest progress lives in the WAL.
        let wal = format!("{INDEX_FILE}-wal");
        if with_wal {
            assert!(std::fs::metadata(source.join(&wal)).unwrap().len() > 32);
        } else {
            drop(writer.take());
        }
        for name in [INDEX_FILE.to_owned(), wal, format!("{INDEX_FILE}-shm")] {
            if name == INDEX_FILE || (with_wal && (with_shm || !name.ends_with("-shm"))) {
                std::fs::copy(source.join(&name), target.join(name)).unwrap();
            }
        }
        if wal_kind == "empty" {
            std::fs::write(target.join(format!("{INDEX_FILE}-wal")), []).unwrap();
        }
        let before = files(&target);
        let mut plan = HistoryPlan::open(&cfg).unwrap();
        assert_eq!(plan.progress.as_ref().unwrap().as_ref(), entries.last());
        for entry in &entries {
            plan.observe(&LogEntry::Command(entry.clone()));
        }
        assert_eq!(files(&target), before, "preflight cannot change even existing SHM bytes");
        let (_guard, mut alerts) = capture_alerts();
        let history = plan.finish().unwrap();
        assert_eq!(history.resume_at(), Offset::start_of(2));
        assert!(alerts.try_recv().is_err(), "valid WAL recovery is ordinary startup");
        drop(history);
        drop(writer);
    }
}

fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (path.file_name().unwrap().to_str().unwrap().to_owned(), std::fs::read(path).unwrap())
        })
        .collect()
}

#[test]
fn empty_database_never_deletes_recovery_evidence_during_preflight() {
    let dir = scratch_dir("history-empty-with-wal");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(INDEX_FILE), []).unwrap();
    std::fs::write(dir.join(format!("{INDEX_FILE}-wal")), b"original recovery evidence").unwrap();
    std::fs::write(dir.join(format!("{INDEX_FILE}-shm")), b"original shared-memory evidence").unwrap();
    let before = files(&dir);
    let plan = HistoryPlan::open(&HistorySection { dir: dir.clone(), retention: Duration::from_secs(60) }).unwrap();
    assert!(plan.progress.unwrap_err().to_string().contains("empty"));
    assert_eq!(files(&dir), before);
}

#[test]
fn invalid_database_geometry_preserves_every_recovery_sidecar() {
    use std::io::{Read, Seek, Write};
    for size in [1, 99, 511, 1_u64 << 41] {
        let dir = scratch_dir(&format!("history-geometry-{size}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(INDEX_FILE);
        let mut header = [0; 100];
        header[..16].copy_from_slice(b"SQLite format 3\0");
        header[16..18].copy_from_slice(&512_u16.to_be_bytes());
        let mut file = std::fs::File::options().create_new(true).read(true).write(true).open(&path).unwrap();
        file.write_all(&header[..usize::try_from(size.min(100)).unwrap()]).unwrap();
        file.set_len(size).unwrap(); // Sparse: never allocate/read the oversized image.
        for suffix in ["-wal", "-shm", "-journal"] {
            std::fs::write(dir.join(format!("{INDEX_FILE}{suffix}")), suffix.as_bytes()).unwrap();
        }
        let plan = HistoryPlan::open(&HistorySection { dir: dir.clone(), retention: Duration::from_secs(60) }).unwrap();
        assert!(plan.progress.is_err(), "unsafe geometry must be rejected before SQLite opens");
        assert_eq!(file.metadata().unwrap().len(), size);
        file.rewind().unwrap();
        let mut after = vec![0; usize::try_from(size.min(100)).unwrap()];
        file.read_exact(&mut after).unwrap();
        assert_eq!(after, header[..after.len()]);
        for suffix in ["-wal", "-shm", "-journal"] {
            assert_eq!(std::fs::read(dir.join(format!("{INDEX_FILE}{suffix}"))).unwrap(), suffix.as_bytes());
        }
    }
}

#[test]
fn geometry_bounds_follow_the_encoded_page_size_without_overflow() {
    let dir = scratch_dir("history-geometry-bounds");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(INDEX_FILE);
    for page_size in [512_u64, 4096, 65_536] {
        let mut header = [0; 100];
        header[..16].copy_from_slice(b"SQLite format 3\0");
        let encoded = if page_size == 65_536 { 1 } else { u16::try_from(page_size).unwrap() };
        header[16..18].copy_from_slice(&encoded.to_be_bytes());
        std::fs::write(&path, header).unwrap();
        let limit = (u64::from(u32::MAX) - 1) * page_size;
        assert!(super::validate_geometry(&path, limit).is_ok());
        assert!(super::validate_geometry(&path, limit + 1).is_err());
        assert!(super::validate_geometry(&path, u64::MAX).is_err());
        assert!(super::validate_geometry(&path, page_size - 1).is_err());
    }
}
