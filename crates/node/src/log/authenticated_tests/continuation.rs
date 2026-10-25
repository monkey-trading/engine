//! Resuming the catalog from its latest signed head across folds and maintenance.

use super::*;

use crate::log::authenticated_tests::head_support::{latest, maintenance_root, resign, trust, writable};
use crate::log::catalog::{DenseSegmentInfo, ordinal_key};
use crate::log::catalog_head::read_catalog_head;
use crate::log::catalog_prefix::extend_catalog_prefix;

#[test]
fn normal_valid_c_resumes_latest_t_after_hundreds_of_folds_and_maintenance_root_advances() {
    let mut f = fixture("catalog-current-tip-hundreds", 1);
    for seq in 0..6 {
        f.writer.append(&command(seq)).expect("initial commands");
    }
    let c = boundary(&mut f, 5);
    for seq in 6..=310 {
        f.writer.append(&command(seq)).expect("post-C command");
        f.writer.sync().expect("authoritative bytes durable");
        f.writer.flush_catalog().expect("signed durable catalog fold");
        if seq % 7 == 0 {
            f.writer.catalog_maintenance(maintenance_root).expect("maintenance root head");
        }
    }
    let t = latest(&f.heads);
    assert!(f.heads.publications().len() > 300);
    assert_eq!(t.catalog().completed_segments, 310);
    assert!(t.catalog().root.version.expect("T version") > c.catalog.root.version.expect("C version") + 300);
    for first in 0..c.catalog.next_seq {
        fs::remove_file(f.dir.join(format!("{first:020}.jsonl"))).expect("old segment removed by host");
    }
    let mut manifest = fs::read(f.dir.join(MANIFEST)).expect("manifest locator");
    manifest[..c.catalog.manifest_bytes as usize].fill(b'x');
    fs::write(f.dir.join(MANIFEST), manifest).expect("unread historical locator prefix altered");
    drop(f.writer);
    let before = fs::read(f.dir.join("catalog.sqlite")).expect("store before read-only opening");
    let mut actual = Vec::new();
    let log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, Some(&t), |entry| {
        if let LogEntry::Command(command) = entry {
            actual.push(command.sequenced.seq);
        }
        Ok(())
    })
    .expect("normal C to current T continuation");
    assert_eq!(actual, (6..=310).collect::<Vec<_>>(), "T never skips physical replay after C");
    assert_eq!(fs::read(f.dir.join("catalog.sqlite")).expect("store after read-only opening"), before);
    let view = log.manifest_view();
    assert!(view.segments.is_empty());
    assert_eq!(view.catalog.as_ref().expect("catalog").certified(), t.catalog());
    assert_eq!(view.completed_len(), 310);
    let log = writable(log, f.store.clone(), f.read.clone(), f.heads.clone());
    let (tx, watch) = watch::channel(Manifest::default());
    let mut writer = log.into_writer(1, tx).expect("post-preflight writer base is T");
    assert_eq!(fs::read(f.dir.join("catalog.sqlite")).expect("no covered descriptors reinserted"), before);
    writer.append(&command(311)).expect("new physical suffix");
    writer.sync().expect("sync new physical bytes");
    writer.flush_catalog().expect("append only the completion after T");
    let next = latest(&f.heads);
    assert_eq!(next.catalog().completed_segments, 311);
    assert!(next.catalog().root.version > t.catalog().root.version);
    assert_eq!(next.certificate().body.nonce, next.catalog().root.version.expect("actual current version"));
    let mut reader = LogReader::open(&f.dir, Offset::start_of(6), watch).expect("lazy backfill");
    assert_eq!(offsets(&mut reader).expect("all messages"), (6..=311).map(Offset::start_of).collect::<Vec<_>>());
}

#[test]
fn catalog_prefix_is_independent_of_fold_batch_sizes_and_gc_only_versions() {
    let mut a = fixture("catalog-batches-a", 1);
    let mut b = fixture("catalog-batches-b", 1);
    for seq in 0..6 {
        a.writer.append(&command(seq)).expect("A initial");
        b.writer.append(&command(seq)).expect("B initial");
    }
    let c = boundary(&mut a, 5);
    let other_c = boundary(&mut b, 5);
    assert_eq!(c.catalog, other_c.catalog);
    for seq in 6..=80 {
        a.writer.append(&command(seq)).expect("A suffix");
        b.writer.append(&command(seq)).expect("B suffix");
        a.writer.flush_catalog().expect("A one-completion folds");
        if seq % 9 == 0 {
            b.writer.flush_catalog().expect("B varied folds");
        }
        if seq % 11 == 0 {
            b.writer.catalog_maintenance(maintenance_root).expect("B maintenance");
        }
    }
    a.writer.sync().expect("A sync");
    b.writer.sync().expect("B sync");
    a.writer.flush_catalog().expect("A final fold");
    b.writer.flush_catalog().expect("B final fold");
    let a_tip = latest(&a.heads);
    let b_tip = latest(&b.heads);
    assert_ne!(a_tip.catalog().root.version, b_tip.catalog().root.version);
    assert_eq!(a_tip.catalog().prefix, b_tip.catalog().prefix);
    assert_eq!(a_tip.catalog().manifest_bytes, b_tip.catalog().manifest_bytes);
    drop(b.writer);
    let mut seen = Vec::new();
    let log = SegmentedLog::open_suffix(&b.dir, b.read.clone(), &c, Some(&b_tip), |entry| {
        if let LogEntry::Command(command) = entry {
            seen.push(command.sequenced.seq);
        }
        Ok(())
    })
    .expect("same C prefix with different fold/maintenance history");
    assert_eq!(seen, (6..=80).collect::<Vec<_>>());
    assert_eq!(log.manifest_view().catalog.as_ref().expect("T base").certified(), b_tip.catalog());
    let log = writable(log, b.store.clone(), b.read.clone(), b.heads.clone());
    let (tx, _) = watch::channel(Manifest::default());
    let mut writer = log.into_writer(1, tx).expect("write base is current B tip");
    writer.append(&command(81)).expect("next command");
    writer.flush_catalog().expect("no old-version collision");
}

#[test]
fn a_pinned_signed_tip_from_a_different_completed_prefix_is_rejected_before_replay() {
    let mut f = fixture("catalog-forked-tip", 1);
    for seq in 0..6 {
        f.writer.append(&command(seq)).expect("initial");
    }
    let c = boundary(&mut f, 5);
    for seq in 6..12 {
        f.writer.append(&command(seq)).expect("suffix");
    }
    f.writer.sync().expect("sync");
    f.writer.flush_catalog().expect("tip");
    let original = latest(&f.heads);
    let catalog = LazyCatalogView::open(f.read.clone(), original.catalog().clone()).expect("tip view");
    let mut different: DenseSegmentInfo = catalog.at(c.catalog.completed_segments - 1).expect("C boundary dense leaf");
    different.info.sha256 = "ab".repeat(32);
    different.manifest_line =
        crate::log::segments::listing_line(&different.info).expect("canonical changed descriptor");
    different.prefix = extend_catalog_prefix(different.previous_prefix, &different);
    let fork = f
        .store
        .update(
            &original.catalog().root,
            vec![(ordinal_key(different.ordinal), Some(serde_json::to_vec(&different).expect("different leaf")))],
        )
        .expect("independent signed fixture root");
    let mut certificate = original.certificate().clone();
    certificate.body.catalog.root = fork.clone();
    certificate.body.nonce = fork.version.expect("fork nonce");
    let verified = resign(certificate).verify(&trust()).expect("signature alone is valid");
    let mut seen = Vec::new();
    let error = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, Some(&verified), |entry| {
        seen.push(entry);
        Ok(())
    })
    .err()
    .expect("different prefix rejected");
    assert!(format!("{error:#}").contains("continuation prefix mismatch"));
    assert!(seen.is_empty());
}

#[test]
fn catalog_tip_signature_domain_signer_nonce_and_prefix_are_independently_checked() {
    let mut f = fixture("catalog-head-auth", 1);
    for seq in 0..4 {
        f.writer.append(&command(seq)).expect("append");
    }
    let _c = boundary(&mut f, 3);
    let good = latest(&f.heads).certificate().clone();
    let mut other_domain = good.clone();
    other_domain.body.domain.chain_id += 1;
    assert!(resign(other_domain).verify(&trust()).is_err(), "pinned signature cannot cross domains");
    // The repository's published dev operator key is distinct from the pinned mock enclave key.
    let other_key = k256::ecdsa::SigningKey::from_bytes((&[0x42; 32]).into()).expect("published development key");
    let mut disk_signer = good.clone();
    disk_signer.signer = engine_core::eip712::address(other_key.verifying_key()).0;
    let digest = crate::log::catalog_head::head_digest(&disk_signer.body).expect("same genuine payload");
    disk_signer.signature =
        format!("0x{}", hex::encode(engine_core::eip712::sign(&other_key, &engine_core::eip712::B256::from(digest))));
    let error = disk_signer.verify(&trust()).expect_err("valid signature from an unpinned signer");
    assert!(format!("{error:#}").contains("unknown catalog head signer"));
    let mut bad_nonce = good.clone();
    bad_nonce.body.nonce += 1;
    assert!(resign(bad_nonce).verify(&trust()).is_err(), "nonce must be the exact Store version");
    let mut bad_prefix = good.clone();
    bad_prefix.body.catalog.prefix[0] ^= 1;
    assert!(bad_prefix.verify(&trust()).is_err(), "altered prefix is signed payload");
    let re_signed_bad = resign(bad_prefix).verify(&trust()).expect("valid fixture signature");
    assert!(
        LazyCatalogView::open(f.read.clone(), re_signed_bad.catalog().clone()).is_err(),
        "metadata must prove the signed prefix"
    );
    let mut bad_format = good;
    bad_format.body.format += 1;
    assert!(resign(bad_format).verify(&trust()).is_err());
}

#[test]
fn unavailable_tip_metadata_is_a_cache_fault_without_any_preflight_mutation() {
    let mut f = fixture("catalog-head-missing-page", 1);
    for seq in 0..6 {
        f.writer.append(&command(seq)).expect("initial");
    }
    let c = boundary(&mut f, 5);
    for seq in 6..12 {
        f.writer.append(&command(seq)).expect("suffix");
    }
    f.writer.sync().expect("sync");
    f.writer.flush_catalog().expect("tip");
    let t = latest(&f.heads);
    let meta = serde_json::to_vec(&t.catalog().meta()).expect("exact metadata bytes");
    let db = rusqlite::Connection::open(f.dir.join("catalog.sqlite")).expect("host writable database");
    assert!(
        db.execute("DELETE FROM value_rows WHERE data=?1", rusqlite::params![meta]).expect("host coherent removal") > 0
    );
    drop(db);
    let before = fs::read(f.dir.join("catalog.sqlite")).expect("fault evidence");
    let mut seen = Vec::new();
    assert!(
        SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, Some(&t), |entry| {
            seen.push(entry);
            Ok(())
        })
        .is_err()
    );
    assert!(seen.is_empty());
    assert_eq!(fs::read(f.dir.join("catalog.sqlite")).expect("preserved evidence"), before);
}

#[test]
fn missing_tip_certificate_recovers_from_the_signed_boundary_never_the_later_sql_root() {
    let mut f = fixture("catalog-head-missing-cert", 1);
    for seq in 0..6 {
        f.writer.append(&command(seq)).expect("initial");
    }
    let c = boundary(&mut f, 5);
    for seq in 6..80 {
        f.writer.append(&command(seq)).expect("post-C");
    }
    f.writer.sync().expect("sync");
    f.writer.flush_catalog().expect("later signed head");
    fs::remove_file(f.dir.join("catalog-head.json")).expect("missing latest certificate");
    assert!(read_catalog_head(f.heads.as_ref(), &trust()).is_err());
    let before = fs::read(f.dir.join("catalog.sqlite")).expect("evidence before recovery");
    let log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, None, |_| Ok(()))
        .expect("the boundary's signed root plus the replayed suffix");
    let catalog = log.manifest_view().catalog.expect("recovered catalog");
    assert_eq!(catalog.certified(), &c.catalog, "the later SQL root is never adopted");
    assert_eq!(fs::read(f.dir.join("catalog.sqlite")).expect("evidence after recovery"), before);
}

#[test]
fn body_time_active_verification_still_checks_actual_chunks_after_t_adoption() {
    use crate::log::checked::{CheckedChunks, check_prefix};
    use std::io::Read;

    let mut f = fixture("catalog-tip-active-body-time", 16);
    for seq in 0..6 {
        f.writer.append(&command(seq)).expect("initial");
    }
    let c = boundary(&mut f, 5);
    for seq in 6..27 {
        f.writer.append(&command(seq)).expect("suffix");
    }
    f.writer.sync().expect("actual bytes durable");
    f.writer.flush_catalog().expect("T covers completed segment zero");
    let t = latest(&f.heads);
    drop(f.writer);
    let log = SegmentedLog::open_suffix(&f.dir, f.read.clone(), &c, Some(&t), |_| Ok(()))
        .expect("strict physical C to tip pass");
    let active = log.manifest_view().active.expect("actual replayed active commitment");
    let path = f.dir.join(&active.info.file);
    let hashes = check_prefix(&path, &active.info).expect("initial complete prefix verification");
    let mut body = CheckedChunks::open(&path, hashes, 0).expect("body opens and checks its first chunk");
    let text = fs::read_to_string(&path).expect("active bytes");
    let changed = text.replacen("\"ts\":25,", "\"ts\":99,", 1);
    assert_ne!(text, changed);
    fs::write(path, changed).expect("host changes a not-yet-read body chunk");
    assert!(body.read_to_end(&mut Vec::new()).is_err(), "no successful body containing the changed active bytes");
}
