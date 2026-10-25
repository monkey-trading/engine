//! Recovered appends refuse changed active bytes and legacy manifest whitespace.

use super::*;

use crate::log::authenticated_tests::head_support::{latest, writable};

#[test]
fn legacy_manifest_whitespace_uses_certified_physical_seek_and_canonical_archive_bytes() {
    let dir = scratch_dir("catalog-legacy-whitespace");
    let (tx, _) = watch::channel(Manifest::default());
    let mut legacy = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(1, tx).unwrap();
    for seq in 0..5 {
        legacy.append(&command(seq)).unwrap();
    }
    legacy.sync().unwrap();
    drop(legacy);
    let path = dir.join(MANIFEST);
    let canonical = fs::read(&path).unwrap();
    let mut raw = Vec::new();
    for line in canonical.split_inclusive(|byte| *byte == b'\n') {
        raw.extend_from_slice(b"  ");
        raw.extend_from_slice(&line[..line.len() - 1]);
        raw.extend_from_slice(b" \r\n");
    }
    fs::write(&path, &raw).unwrap();
    let store = Arc::new(Store::create(dir.join("catalog.sqlite")).unwrap());
    let read = access(store.clone(), dir.join("catalog.sqlite"));
    let heads = Arc::new(HeadFiles::new(&dir));
    let owner =
        CatalogOwner::create(store.clone(), read.clone(), Root::empty([7; 32]), authority(heads.clone())).unwrap();
    let recovered = SegmentedLog::open(&dir, |_| Ok(())).unwrap().attach_catalog(owner).unwrap();
    let (tx, view) = watch::channel(Manifest::default());
    let mut writer = recovered.into_writer(1, tx).unwrap();
    writer.append(&command(5)).unwrap();
    writer.append(&checkpoint(5)).unwrap();
    writer.sync().unwrap();
    let (c, pinned) = writer.seal_catalog_boundary().unwrap();
    assert!(c.manifest_seek > c.catalog.manifest_bytes, "legacy physical and public offsets really differ");
    assert_eq!(view.borrow().to_jsonl().unwrap(), pinned.to_jsonl().unwrap());
    assert_eq!(&fs::read(&path).unwrap()[..raw.len()], raw, "migration never rewrites legacy prefix");
    for seq in 6..8 {
        writer.append(&command(seq)).unwrap();
    }
    writer.sync().unwrap();
    writer.flush_catalog().unwrap();
    let t = latest(&heads);
    let expected_listing = view.borrow().to_jsonl().unwrap();
    let mut damaged_old_prefix = fs::read(&path).unwrap();
    damaged_old_prefix[..c.manifest_seek as usize].fill(b'x');
    fs::write(&path, damaged_old_prefix).unwrap();
    drop(writer);
    let mut commands = Vec::new();
    let log = SegmentedLog::open_suffix(&dir, read.clone(), &c, Some(&t), |entry| {
        if let LogEntry::Command(entry) = entry {
            commands.push(entry.sequenced.seq);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(commands, [6, 7]);
    assert_eq!(log.manifest_view().to_jsonl().unwrap(), expected_listing);
    let log = writable(log, store, read, heads);
    let mut writer = log.into_writer(1, watch::channel(Manifest::default()).0).unwrap();
    writer.append(&command(8)).unwrap();
    writer.sync().unwrap();
    assert!(fs::read(path).unwrap().starts_with(&vec![b'x'; c.manifest_seek as usize]));
}

#[test]
fn recovered_append_refuses_changed_active_bytes_before_manifest_trim_or_catalog_mutation() {
    for length_change in [false, true] {
        let dir = scratch_dir("append-same-prefix-check");
        let mut writer = SegmentedLog::open(&dir, |_| Ok(()))
            .unwrap()
            .into_writer(4, watch::channel(Manifest::default()).0)
            .unwrap();
        writer.append(&command(0)).unwrap();
        writer.sync().unwrap();
        drop(writer);
        let log = SegmentedLog::open(&dir, |_| Ok(())).unwrap();
        host_append(&dir.join(MANIFEST), b"interrupted");
        let active = dir.join("00000000000000000000.jsonl");
        let mut bytes = fs::read(&active).unwrap();
        if length_change {
            bytes.push(b'\n');
        } else {
            let i = bytes.iter().position(|b| *b == b'0').unwrap();
            bytes[i] = b'1';
        }
        fs::write(&active, &bytes).unwrap();
        let manifest_before = fs::read(dir.join(MANIFEST)).unwrap();
        let error = log.into_writer(4, watch::channel(Manifest::default()).0).err().unwrap();
        assert!(format!("{error:#}").contains("recovered active log"), "{error:#}");
        assert_eq!(fs::read(&active).unwrap(), bytes);
        assert_eq!(
            fs::read(dir.join(MANIFEST)).unwrap(),
            manifest_before,
            "refusal precedes trimming interrupted evidence"
        );
    }
}
