//! Catalog lookups and suffix recovery from an exact checkpoint boundary.

use super::*;

#[test]
fn direct_boundary_recovery_needs_no_historical_files_or_manifest_prefix_parse() {
    let mut f = fixture("catalog-direct", 2);
    for seq in 0..10 {
        f.writer.append(&command(seq)).expect("append");
    }
    let at = boundary(&mut f, 9);
    let canonical = f.view.borrow().to_jsonl().expect("canonical manifest");
    assert_eq!(at.catalog.completed_segments, 4);
    assert_eq!(at.catalog.next_seq, 8);
    assert_eq!(at.catalog.manifest_bytes, canonical.len() as u64);
    for first in [0, 2, 4, 6] {
        fs::remove_file(f.dir.join(format!("{first:020}.jsonl"))).expect("remove old file");
    }
    fs::write(f.dir.join(MANIFEST), vec![b'x'; canonical.len()]).expect("host replaces old locator bytes");
    let mut visited = Vec::new();
    let log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &at, None, |entry| {
        visited.push(entry);
        Ok(())
    })
    .expect("direct suffix");
    assert!(visited.is_empty());
    assert!(log.manifest_view().segments.is_empty());
    assert_eq!(log.manifest_view().completed_len(), 4);
    assert_eq!(log.manifest_view().to_jsonl().expect("listing from verified leaves"), canonical);
    assert!(crate::sequencer::replay_path(&f.dir).is_err(), "full audit still needs the removed segments");
}

/// A host-appended command claiming the boundary's seq (a duplicate) or an earlier one (reversed)
/// still reaches replay, which is what rejects it.
#[test]
fn suffix_replays_a_physical_entry_even_when_its_claimed_seq_goes_back_to_or_before_boundary_seq() {
    for (at_seq, forged) in [(0, 0), (1, 0)] {
        let mut f = fixture(&format!("catalog-backwards-{at_seq}"), 100);
        for seq in 0..=at_seq {
            f.writer.append(&command(seq)).expect("append");
        }
        let at = boundary(&mut f, at_seq);
        f.writer.append(&command(at_seq + 1)).expect("append suffix");
        f.writer.flush().expect("flush suffix");
        host_append(&f.dir.join("00000000000000000000.jsonl"), &line(&command(forged)));
        let mut seen = Vec::new();
        let result = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &at, None, |entry| {
            if let LogEntry::Command(c) = entry {
                seen.push(c.sequenced.seq);
                anyhow::ensure!(c.sequenced.seq > at_seq, "physical backwards seq reached replay");
            }
            Ok(())
        });
        assert_eq!(seen, [at_seq + 1, forged]);
        assert!(
            format!("{:#}", result.err().expect("replay rejects reverse seq"))
                .contains("physical backwards seq reached replay")
        );
    }
}

#[test]
fn exact_checkpoint_framing_count_and_manifest_boundary_cannot_be_forged() {
    let mut f = fixture("catalog-framing", 2);
    for seq in 0..5 {
        f.writer.append(&command(seq)).expect("append");
    }
    let at = boundary(&mut f, 4);
    for mutation in 0..5 {
        let mut forged = at.clone();
        match mutation {
            0 => forged.checkpoint.start += 1,
            1 => forged.checkpoint.end -= 1,
            2 => forged.catalog.completed_segments -= 1,
            3 => forged.catalog.manifest_bytes -= 1,
            _ => forged.prefix.info.sha256 = "ff".repeat(32),
        }
        let mut seen = Vec::new();
        assert!(
            SegmentedLog::open_suffix(&f.dir, f.read.clone(), &forged, None, |entry| {
                seen.push(entry);
                Ok(())
            })
            .is_err(),
            "mutation {mutation}"
        );
        assert!(seen.is_empty(), "no suffix before boundary authentication: {mutation}");
    }
}

#[test]
fn lazy_lookups_cover_every_ordinal_across_multiple_durable_folds() {
    let mut f = fixture("catalog-folds", 1);
    for seq in 0..150 {
        f.writer.append(&command(seq)).expect("append");
    }
    let at = boundary(&mut f, 149);
    let view = f.view.borrow().clone();
    assert!(view.segments.is_empty());
    assert_eq!(view.completed_len(), 149);
    let catalog = LazyCatalogView::open(f.read.clone(), at.catalog.clone()).expect("reopen fixed catalog root");
    for seq in 0..149 {
        let dense = catalog.lookup_seq(seq).expect("verified probe").expect("range exists");
        assert_eq!((dense.ordinal, dense.info.first_seq, dense.info.last_seq), (seq, seq, seq));
    }
    assert!(catalog.lookup_seq(149).expect("verified upper bound").is_none());
    assert!(catalog.lookup_seq(u64::MAX).expect("verified upper bound").is_none());
    assert_eq!(view.to_jsonl().expect("explicit listing"), fs::read(f.dir.join(MANIFEST)).expect("writer bytes"));
}

#[test]
fn a_missing_required_dense_leaf_cannot_shorten_a_successful_lookup() {
    let mut f = fixture("catalog-missing-rank", 1);
    for seq in 0..4 {
        f.writer.append(&command(seq)).expect("append");
    }
    let at = boundary(&mut f, 3);
    let key = [b"log/catalog/v1/segment/".as_slice(), &1u64.to_be_bytes()].concat();
    let root = f.store.update(&at.catalog.root, vec![(key, None)]).expect("create malformed test root");
    let mut forged = at.catalog;
    forged.root = root;
    let catalog = LazyCatalogView::open(f.read.clone(), forged).expect("metadata remains present");
    let error = catalog.lookup_seq(1).expect_err("required ordinal missing");
    assert!(format!("{error:#}").contains("missing required catalog leaf"));
}

#[test]
fn authenticated_absence_of_the_exact_checkpoint_invalidates_the_boundary() {
    let mut f = fixture("catalog-missing-checkpoint", 100);
    f.writer.append(&command(0)).expect("append");
    let mut at = boundary(&mut f, 0);
    let offset = at.checkpoint.offset;
    let key =
        [b"log/catalog/v1/checkpoint/".as_slice(), &offset.seq.to_be_bytes(), &offset.index.to_be_bytes()].concat();
    at.catalog.root = f.store.update(&at.catalog.root, vec![(key, None)]).expect("test deletion root");
    let result = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &at, None, |_| Ok(()));
    assert!(
        format!("{:#}", result.err().expect("missing exact checkpoint"))
            .contains("exact checkpoint locator proof mismatch")
    );
}

#[test]
fn suffix_crosses_rotations_and_reconstructs_the_actual_open_segment_for_append() {
    let mut f = fixture("catalog-suffix-rotation", 2);
    f.writer.append(&command(0)).expect("append");
    let at = boundary(&mut f, 0);
    for seq in 1..8 {
        f.writer.append(&command(seq)).expect("suffix append");
    }
    f.writer.sync().expect("sync");
    drop(f.writer);
    let mut seen = Vec::new();
    let log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &at, None, |entry| {
        if let LogEntry::Command(c) = entry {
            seen.push(c.sequenced.seq);
        }
        Ok(())
    })
    .expect("suffix recovery");
    assert_eq!(seen, [1, 2, 3, 4, 5, 6, 7]);
    assert!(log.manifest_view().segments.is_empty());
    let (tx, view) = watch::channel(Manifest::default());
    let log = head_support::writable(log, f.store.clone(), f.read.clone(), f.heads.clone());
    let mut writer = log.into_writer(2, tx).expect("resume writer");
    writer.append(&command(8)).expect("rotation from recovered real counters");
    writer.sync().expect("sync appended");
    let mut reader = LogReader::open(&f.dir, Offset::start_of(0), view).expect("reader");
    assert_eq!(
        offsets(&mut reader).expect("complete backfill"),
        [
            Offset::start_of(0),
            Offset { seq: 0, index: 1 },
            Offset::start_of(1),
            Offset::start_of(2),
            Offset::start_of(3),
            Offset::start_of(4),
            Offset::start_of(5),
            Offset::start_of(6),
            Offset::start_of(7),
            Offset::start_of(8)
        ]
    );
}

#[test]
fn orphan_files_are_not_catalog_authority_and_rotation_never_overwrites_them() {
    let mut f = fixture("catalog-orphan", 2);
    f.writer.append(&command(0)).expect("append");
    let at = boundary(&mut f, 0);
    let planted = f.dir.join("00000000000000000002.jsonl");
    fs::write(&planted, b"orphan evidence\n").expect("plant orphan");
    let log =
        SegmentedLog::open_suffix(&f.dir, f.read.clone(), &at, None, |_| Ok(())).expect("unreferenced orphan ignored");
    assert_eq!(log.manifest_view().completed_len(), 0);
    assert!(SegmentedLog::open(&f.dir, |_| Ok(())).is_err());
    f.writer.append(&command(1)).expect("fill active segment");
    assert!(f.writer.append(&command(2)).is_err());
    assert_eq!(fs::read(planted).expect("orphan remains"), b"orphan evidence\n");
}

#[test]
fn a_long_suffix_is_fully_replayed_readonly_then_prepared_before_consumers_are_published() {
    let mut f = fixture("catalog-long-suffix", 1);
    f.writer.append(&command(0)).expect("append");
    let at = boundary(&mut f, 0);
    for seq in 1..140 {
        f.writer.append(&command(seq)).expect("append long suffix");
    }
    f.writer.sync().expect("sync");
    drop(f.writer);
    let before = fs::read(f.dir.join("catalog.sqlite")).expect("store before read-only replay");
    let tip = head_support::latest(&f.heads);
    let mut seen = Vec::new();
    let log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &at, Some(&tip), |entry| {
        if let LogEntry::Command(c) = entry {
            seen.push(c.sequenced.seq);
        }
        Ok(())
    })
    .expect("all uncovered physical segments replayed");
    assert_eq!(seen, (1..140).collect::<Vec<_>>());
    assert_eq!(fs::read(f.dir.join("catalog.sqlite")).expect("store after read-only replay"), before);
    assert_eq!(log.manifest_view().completed_len(), 139, "current tip plus all uncovered completions");
    assert!(log.manifest_view().segments.is_empty(), "no historical descriptor hydration");
    let (tx, view) = watch::channel(Manifest::default());
    let log = head_support::writable(log, f.store.clone(), f.read.clone(), f.heads.clone());
    let _writer = log.into_writer(1, tx).expect("authorized catalog preparation");
    assert_eq!(view.borrow().completed_len(), 139);
    assert!(view.borrow().segments.is_empty());
    let mut reader = LogReader::open(&f.dir, Offset::start_of(138), view).expect("prepared reader");
    assert_eq!(offsets(&mut reader).expect("last sequences"), [Offset::start_of(138), Offset::start_of(139)]);
}

/// The archive keeps the listing it last proved and proves only segments completed since: damage
/// to an already listed ordinal fails a full rebuild but neither the unchanged listing nor its
/// extension.
#[test]
fn the_archive_listing_proves_only_segments_completed_since_it_was_cached() {
    let mut f = fixture("catalog-listing-cache", 1);
    for seq in 0..4 {
        f.writer.append(&command(seq)).expect("command");
    }
    f.writer.sync().expect("log sync");
    f.writer.flush_catalog().expect("fold");
    let first = f.view.borrow().catalog.clone().expect("catalog");
    let cached = first.listing_after(None).expect("full listing");
    assert_eq!(cached.bytes.to_vec(), first.to_jsonl().expect("listing"));
    let listed = serde_json::to_vec(&first.at(0).expect("ordinal 0")).expect("encoding");
    let db = rusqlite::Connection::open(f.dir.join("catalog.sqlite")).expect("host database");
    assert!(db.execute("DELETE FROM value_rows WHERE data = ?1", [listed]).expect("host damage") > 0);
    drop(db);
    let again = first.listing_after(Some(&cached)).expect("nothing completed since");
    assert!(Arc::ptr_eq(&again.bytes, &cached.bytes), "an unchanged listing is neither proved nor copied");
    for seq in 4..7 {
        f.writer.append(&command(seq)).expect("command");
    }
    f.writer.sync().expect("log sync");
    f.writer.flush_catalog().expect("fold");
    let later = f.view.borrow().catalog.clone().expect("catalog");
    assert!(later.to_jsonl().is_err(), "a full listing reads the damaged ordinal");
    let extended = later.listing_after(Some(&cached)).expect("only the new segments are proved");
    assert!(extended.bytes.starts_with(&cached.bytes[..]));
    assert_eq!(extended.bytes.len() as u64, later.manifest_bytes());
}
