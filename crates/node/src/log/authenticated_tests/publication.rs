//! Catalog commits and signed head publication under storage and sync failures.

use super::*;

use crate::log::authenticated_tests::head_support::{latest, maintenance_root, trust, writable};
use crate::log::catalog::ordinal_key;
use crate::log::catalog_head::{CatalogHeadCertificate, read_catalog_head};

/// A crash after a catalog commit but before its head was signed leaves a version recovery never
/// reads: it recovers at the signed tip without changing a byte, and only activation, after the
/// preflight, discards the later version so writes resume from the tip.
#[test]
fn an_unacknowledged_store_commit_is_read_past_before_preflight_and_discarded_at_activation() {
    let mut f = fixture("catalog-unacknowledged-commit", 1);
    for seq in 0..6 {
        f.writer.append(&command(seq)).expect("initial commands");
    }
    let c = boundary(&mut f, 5);
    let acknowledged = latest(&f.heads);
    let unacknowledged =
        maintenance_root(&f.store, &acknowledged.catalog().root).expect("durable Store commit before missing head ack");
    assert!(unacknowledged.version > acknowledged.catalog().root.version);
    let (dir, heads) = (f.dir.clone(), f.heads.clone());
    drop(f);
    let path = dir.join("catalog.sqlite");
    let before = fs::read(&path).expect("unacknowledged commit evidence");
    let store = Arc::new(Store::open_existing(&path).expect("restart opens the store read-only"));
    let read = access(store.clone(), path.clone());
    let log = SegmentedLog::open_suffix(&dir, read.clone(), &c, Some(&acknowledged), |_| Ok(()))
        .expect("recovery reads at the signed tip, never the later root");
    assert_eq!(log.manifest_view().catalog.expect("recovered catalog").certified(), acknowledged.catalog());
    assert_eq!(fs::read(&path).expect("no preflight repair"), before);
    store.enable_writes().expect("writes after preflight");
    store.discard_after(&acknowledged.catalog().root).expect("the unacknowledged version is discarded");
    let mut writer = writable(log, store, read, heads.clone())
        .into_writer(1, watch::channel(Manifest::default()).0)
        .expect("writer");
    writer.append(&command(6)).expect("next command");
    writer.sync().expect("log sync");
    writer.flush_catalog().expect("the next fold writes after the signed tip");
    assert!(latest(&heads).catalog().root.version > acknowledged.catalog().root.version);
}

#[test]
fn failed_head_publication_preserves_the_stable_tip_and_retries_without_reinserting_versions() {
    let mut f = fixture("catalog-head-retry", 1);
    f.writer.append(&command(0)).expect("command");
    let c = boundary(&mut f, 0);
    let previous = latest(&f.heads);
    f.writer.append(&command(1)).expect("complete segment zero");
    f.writer.sync().expect("authoritative bytes");
    f.heads.fail_at(1);
    assert!(f.writer.flush_catalog().is_err());
    assert_eq!(latest(&f.heads).catalog(), previous.catalog());
    assert_eq!(f.view.borrow().catalog.as_ref().expect("visible view").certified(), &c.catalog);
    let committed = f.writer.catalog_owner.as_ref().expect("owner").write_root().clone();
    assert!(committed.version > previous.catalog().root.version);
    let store_bytes = fs::read(f.dir.join("catalog.sqlite")).expect("committed Store bytes");
    f.heads.fail_at(0);
    f.writer.flush_catalog().expect("publish exact pending root, then trim its covered live delta");
    assert_eq!(latest(&f.heads).catalog().root, committed);
    assert_eq!(fs::read(f.dir.join("catalog.sqlite")).expect("retry Store bytes"), store_bytes);
    assert_eq!(f.view.borrow().catalog.as_ref().expect("published").certified().completed_segments, 1);
    let retained: CatalogHeadCertificate =
        serde_json::from_slice(&fs::read(f.dir.join("catalog-head.previous.json")).expect("prior stable slot"))
            .expect("prior certificate");
    assert_eq!(retained, previous.certificate().clone());
    f.writer.append(&command(2)).expect("ordinary next append");
    f.writer.flush_catalog().expect("current root next update");
    assert!(latest(&f.heads).catalog().root.version > committed.version);
}

#[test]
fn file_or_directory_sync_failures_keep_prior_stable_tip_evidence_and_no_false_ack() {
    for phase in [2, 3] {
        let mut f = fixture(&format!("catalog-head-sync-failure-{phase}"), 1);
        f.writer.append(&command(0)).expect("command");
        let c = boundary(&mut f, 0);
        let previous = latest(&f.heads);
        f.writer.append(&command(1)).expect("rotation");
        f.writer.sync().expect("log sync");
        f.heads.fail_at(phase);
        assert!(f.writer.flush_catalog().is_err());
        assert_eq!(f.view.borrow().catalog.as_ref().expect("visible stable root").certified(), &c.catalog);
        let retained: CatalogHeadCertificate = serde_json::from_slice(
            &fs::read(f.dir.join("catalog-head.previous.json")).expect("prior durable certificate remains"),
        )
        .expect("prior certificate decodes");
        assert_eq!(retained, previous.certificate().clone());
        assert!(retained.verify(&trust()).is_ok());
        // A rename lost to power failure leaves only C signed: recovery reads at C, never the later
        // committed root, and changes nothing before the preflight.
        fs::remove_file(f.dir.join("catalog-head.json")).expect("simulate unavailable latest slot");
        assert!(read_catalog_head(f.heads.as_ref(), &trust()).is_err());
        let before = fs::read(f.dir.join("catalog.sqlite")).expect("evidence");
        let log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, None, |_| Ok(())).expect("recovery at C");
        assert_eq!(log.manifest_view().catalog.expect("recovered catalog").certified(), &c.catalog);
        assert_eq!(fs::read(f.dir.join("catalog.sqlite")).expect("no preflight mutation"), before);
    }
}

/// A fold commits in stages; when a later stage rolls back, the retry finds the ordinals an
/// earlier stage committed and keeps them if they match byte for byte, refusing any that differ.
#[test]
fn a_retried_fold_keeps_identical_ordinals_an_earlier_stage_committed() {
    for conflicting in [false, true] {
        let mut f = fixture(&format!("catalog-retried-fold-{conflicting}"), 1);
        f.writer.append(&command(0)).expect("command");
        boundary(&mut f, 0);
        f.writer.append(&command(1)).expect("rotation");
        f.writer.sync().expect("log sync");
        let view = f.view.borrow().catalog.clone().expect("live catalog");
        let mut dense = view.delta.first().expect("a completed segment waits in memory").dense.clone();
        if conflicting {
            dense.info.bytes += 1;
        }
        let committed = (ordinal_key(dense.ordinal), Some(serde_json::to_vec(&dense).expect("encoding")));
        f.writer.catalog_owner.as_mut().expect("owner").stage(vec![committed]).expect("the earlier stage");
        let retried = f.writer.flush_catalog();
        assert_eq!(retried.is_ok(), !conflicting, "{retried:?}");
        if !conflicting {
            assert_eq!(latest(&f.heads).catalog().completed_segments, 1);
        }
    }
}

#[test]
fn a_catalog_write_that_rolls_back_leaves_the_owner_writable() {
    let mut f = fixture("catalog-rolled-back-stage", 1);
    let sidecar = f.dir.join("catalog.sqlite-wal");
    fs::write(&sidecar, b"transient host fault").expect("sidecar");
    let owner = f.writer.catalog_owner.as_mut().expect("owner");
    let change = || vec![(b"probe".to_vec(), Some(b"value".to_vec()))];
    assert!(owner.stage(change()).is_err(), "the store refuses the write before its transaction");
    assert!(!f.store.requires_reopen(), "nothing was committed");
    fs::remove_file(&sidecar).expect("the fault clears");
    owner.stage(change()).expect("the owner retries from the same root");
}

#[test]
fn a_failing_catalog_warns_once_and_keeps_rotating_below_its_live_capacity() {
    let mut f = fixture("catalog-failing-rotation", 1);
    f.writer.append(&command(0)).expect("command");
    boundary(&mut f, 0);
    f.heads.fail_at(1);
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    f.writer.append(&command(1)).expect("rotation");
    f.writer.sync().expect("log sync");
    assert!(f.writer.flush_catalog().is_err(), "the committed root's head is not published");
    for seq in 2..10 {
        f.writer.append(&command(seq)).expect("rotation while publication fails");
    }
    f.writer.flush().expect("flush");
    let active = f.view.borrow().active.as_ref().expect("active prefix").info.first_seq;
    assert_eq!(active, 9, "every command still starts its own segment");
    let warnings = std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|alert| alert["alert"] == "log_catalog_unavailable")
        .count();
    assert_eq!(warnings, 1, "one warning for the whole outage");
    f.heads.fail_at(0);
    f.writer.append(&command(10)).expect("rotation after the outage");
    f.writer.sync().expect("log sync");
    f.writer.flush_catalog().expect("the held segments are published");
    assert_eq!(latest(&f.heads).catalog().completed_segments, 10);
}

#[test]
fn a_thousand_checkpoints_in_one_segment_stage_below_store_caps_before_one_visible_head() {
    let mut f = fixture("catalog-thousand-locators", 1000);
    for seq in 0..1000 {
        f.writer.append(&command(seq)).expect("command");
        f.writer.append(&checkpoint(seq)).expect("checkpoint every command");
    }
    f.writer.sync().expect("large active segment durable");
    let initial = latest(&f.heads);
    let watched = f.view.clone();
    let read = f.read.clone();
    f.heads.observe(Arc::new(move |certificate| {
        if certificate.body.catalog.completed_segments == 1 {
            assert_eq!(watched.borrow().catalog.as_ref().expect("old visible root").certified().completed_segments, 0);
            let view = LazyCatalogView::open(read.clone(), certificate.body.catalog.clone())?;
            assert_eq!(view.at(0)?.locators.count, 1000);
            for seq in 0..1000 {
                let locator = view.checkpoint(Offset { seq, index: 1 })?.expect("no omitted locator at publication");
                assert_eq!(locator.checkpoint.seq, seq);
            }
        }
        Ok(())
    }));
    f.writer.append(&command(1000)).expect("complete the thousand-command segment");
    f.writer.sync().expect("rotation durable");
    assert_eq!(f.view.borrow().completed_len(), 1, "trusted live completion exists before optional fold");
    assert_eq!(f.view.borrow().catalog.as_ref().expect("stable base").certified().completed_segments, 0);
    f.writer.flush_catalog().expect("bounded locator stages, final metadata, durable signed head");
    let t = latest(&f.heads);
    assert_eq!(t.catalog().completed_segments, 1);
    assert!(t.catalog().root.version.expect("T") >= initial.catalog().root.version.expect("initial") + 4);
    assert_eq!(f.heads.publications().iter().filter(|h| h.body.catalog.completed_segments == 1).count(), 1);
    let view = LazyCatalogView::open(f.read.clone(), t.catalog().clone()).expect("durable completed view");
    for seq in 0..1000 {
        assert_eq!(
            view.checkpoint(Offset { seq, index: 1 }).expect("official proof").expect("locator").checkpoint.seq,
            seq
        );
    }
    assert_eq!(
        f.view.borrow().to_jsonl().expect("canonical archive manifest"),
        fs::read(f.dir.join(MANIFEST)).expect("writer manifest bytes")
    );
    assert!(f.writer.seal_catalog_boundary().is_err(), "catalog T is not an engine/history checkpoint acknowledgment");
}

#[test]
fn an_interrupted_locator_stage_never_exposes_a_partial_segment_as_completed() {
    let mut f = fixture("catalog-interrupted-locator-stage", 1000);
    for seq in 0..1000 {
        f.writer.append(&command(seq)).expect("command");
        f.writer.append(&checkpoint(seq)).expect("checkpoint");
    }
    f.writer.append(&command(1000)).expect("rotation");
    f.writer.sync().expect("log durable");
    let stable = latest(&f.heads);
    let completion = f.view.borrow().catalog.as_ref().expect("live").delta[0].clone();
    let changes = completion
        .checkpoints
        .iter()
        .take(crate::authenticated::MAX_UPDATES)
        .map(|locator| {
            (
                crate::log::catalog::checkpoint_key(locator.offset),
                Some(serde_json::to_vec(locator).expect("locator bytes")),
            )
        })
        .collect();
    let stage = f.store.update(&stable.catalog().root, changes).expect("first bounded internal locator stage only");
    assert!(stage.version > stable.catalog().root.version);
    let mut staged_identity = stable.catalog().clone();
    staged_identity.root = stage;
    let incomplete = LazyCatalogView::open(f.read.clone(), staged_identity)
        .expect("metadata still describes prior completed prefix");
    assert_eq!(incomplete.len(), 0);
    assert!(incomplete.lookup_seq(0).expect("completed range absence").is_none());
    assert_eq!(latest(&f.heads).catalog().completed_segments, 0);
    assert_eq!(f.view.borrow().catalog.as_ref().expect("visible root").certified().completed_segments, 0);
    // There is no signed logical completion or new head; restart must take the fault slow path.
    assert!(f.read.ensure_current(&stable.catalog().root).is_err());
}

#[test]
fn a_delayed_exact_c_seal_cannot_acknowledge_a_later_t_as_c() {
    let mut f = fixture("catalog-exact-c-fence", 1);
    f.writer.append(&command(0)).expect("command C");
    f.writer.append(&checkpoint(0)).expect("checkpoint C");
    f.writer.sync().expect("physical C durable");
    let captured = f.writer.capture_catalog_boundary().expect("immutable C capture");
    assert_eq!(captured.checkpoint().offset, Offset { seq: 0, index: 1 });
    f.writer.append(&command(1)).expect("post-C command");
    f.writer.sync().expect("post-C bytes durable");
    f.writer.flush_catalog().expect("T has already passed C's completed-prefix fence");
    let t = latest(&f.heads);
    let before = fs::read(f.dir.join("catalog.sqlite")).expect("current T evidence");
    assert!(
        captured.commit(f.writer.catalog_owner.as_mut().expect("owner")).is_err(),
        "later T is never substituted for a C acknowledgment"
    );
    assert_eq!(latest(&f.heads).catalog(), t.catalog());
    assert_eq!(fs::read(f.dir.join("catalog.sqlite")).expect("no late seal mutations"), before);
}
