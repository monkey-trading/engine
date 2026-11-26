//! A member's snapshot start choosing among the snapshots its origins list.

use super::*;

/// The newest snapshot the member's origins list that passes is taken, fetched, kept and listed; a newer one that is
/// refused is passed over and its fetched file removed; and with no origin answering, the start pages.
#[tokio::test]
async fn the_newest_snapshot_that_passes_is_fetched_and_a_refused_one_removed() {
    let (dir, origin) = (scratch_dir("build-snapshot-fetch"), scratch_dir("build-snapshot-fetch-origin"));
    write_log(&dir).await;
    let older = publish_at(&dir, &origin, &recorded(), |held| held / 3);
    let newer = publish_at(&dir, &origin, &recorded(), |held| 2 * held / 3);
    assert!(older < newer);
    // The newer snapshot's state is not the one its signature covers, though its listing matches its bytes.
    let mut listed = read_manifest(&origin).unwrap();
    let damaged = listed.last_mut().unwrap();
    let path = origin.join(&damaged.file);
    let mut written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    written["engine"]["accounts"].as_object_mut().unwrap().clear();
    let bytes = serde_json::to_vec(&written).unwrap();
    (damaged.bytes, damaged.sha256) = (bytes.len() as u64, hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes)));
    std::fs::write(&path, bytes).unwrap();
    let lines: String = listed.iter().map(|listing| serde_json::to_string(listing).unwrap() + "\n").collect();
    std::fs::write(origin.join(MANIFEST), lines).unwrap();
    let member = scratch_dir("build-snapshot-fetch-member");
    let client = crate::standby::archive::archive_client(false).unwrap();
    let (_guard, mut alerts) = capture_alerts();
    let origins = [serve(&origin).await];
    let signed = snapshot_start(&client, &origins, (&member, &dir), DOMAIN, &[signer()].into(), (u64::MAX, None)).await;
    assert_eq!(signed.map(|signed| signed.snapshot.offset.seq), Some(older));
    let alert = alerts.try_recv().expect("the refusal is paged");
    assert_eq!(
        (alert["alert"].as_str(), alert["seq"].as_str()),
        ("snapshot_start_refused", newer.to_string().as_str())
    );
    let kept: Vec<u64> = read_manifest(&member).unwrap().iter().map(|listing| listing.seq).collect();
    assert_eq!(kept, [older], "the snapshot taken is listed");
    let files =
        std::fs::read_dir(&member).unwrap().filter(|file| file.as_ref().unwrap().path() != member.join(MANIFEST));
    assert_eq!(files.count(), 1, "the refused one's file is removed");

    let (unreachable, empty, signers) =
        (["http://127.0.0.1:1".to_owned()], scratch_dir("build-none"), [signer()].into());
    let none = snapshot_start(&client, &unreachable, (&empty, &dir), DOMAIN, &signers, (u64::MAX, None));
    assert!(none.await.is_none());
    let alert = alerts.try_recv().expect("the silence is paged");
    assert!(alert["message"].contains("no origin's daily snapshot manifest could be read"), "{alert:?}");
}

/// A snapshot from the turns of the member in office is not taken: one listed as stamped in them is not fetched,
/// and one listed otherwise is refused once its state's stamp shows it, paged, and its fetched file removed; and a
/// held snapshot that is refused gives way to an origin's at its offset, which replaces it.
#[tokio::test]
async fn a_snapshot_from_the_in_office_members_turns_is_not_taken() {
    let (dir, origin) = (scratch_dir("build-snapshot-turns"), scratch_dir("build-snapshot-turns-origin"));
    write_log(&dir).await;
    let older = publish_at(&dir, &origin, &recorded(), |held| held / 3);
    publish_at(&dir, &origin, &recorded(), |held| 2 * held / 3);
    // The newer snapshot's state is stamped an hour on, as its signer vouches, while its listing says otherwise.
    let mut listed = read_manifest(&origin).unwrap();
    let newer = listed.pop().unwrap();
    let path = origin.join(&newer.file);
    let mut written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let turns_began = written["engine"]["now"].as_i64().unwrap() + 3_600_000;
    written["engine"]["now"] = serde_json::json!(turns_began);
    let checkpoint: Checkpoint = serde_json::from_value(written["checkpoint"].clone()).unwrap();
    let engine: Engine = serde_json::from_value(written["engine"].take()).unwrap();
    let engine = Encoding::Encoded(serde_json::to_string(&engine).unwrap());
    std::fs::remove_file(&path).unwrap();
    write_listings(&origin, &listed);
    let offset = Offset { seq: newer.seq, index: newer.index };
    publish_now(&origin, Job::bare(offset, checkpoint, newer.ts, engine, recorded())).unwrap();
    let (client, origins) = (crate::standby::archive::archive_client(false).unwrap(), [serve(&origin).await]);
    let (signers, bounds) = ([signer()].into(), (u64::MAX, Some(turns_began)));
    let (_guard, mut alerts) = capture_alerts();

    let member = scratch_dir("build-snapshot-turns-member");
    let signed = snapshot_start(&client, &origins, (&member, &dir), DOMAIN, &signers, bounds).await;
    assert_eq!(signed.map(|signed| signed.snapshot.offset.seq), Some(older), "the one before the turns");
    let alert = alerts.try_recv().expect("the refusal is paged");
    assert!(alert["message"].contains("from the turns of the member in office"), "{alert:?}");
    assert!(!member.join(&newer.file).exists(), "the refused one's file is removed");

    // Listed as it is stamped, it is not fetched; and a damaged held copy of the older one gives way to the origin's.
    let mut truthful = read_manifest(&origin).unwrap();
    truthful.last_mut().unwrap().ts = turns_began;
    write_listings(&origin, &truthful);
    let held = scratch_dir("build-snapshot-turns-held");
    std::fs::create_dir_all(&held).unwrap();
    let kept = truthful.first().unwrap().clone();
    let mut damaged: serde_json::Value =
        serde_json::from_slice(&std::fs::read(origin.join(&kept.file)).unwrap()).unwrap();
    damaged["engine"]["accounts"].as_object_mut().unwrap().clear();
    write_snapshot(&held.join(&kept.file), &damaged);
    write_listings(&held, std::slice::from_ref(&kept));
    let signed = snapshot_start(&client, &origins, (&held, &dir), DOMAIN, &signers, bounds).await;
    assert_eq!(signed.map(|signed| signed.snapshot.offset.seq), Some(older), "the origin's copy of the older one");
    let alert = alerts.try_recv().expect("the held one's refusal is paged");
    assert!(alert["message"].contains("signature"), "{alert:?}");
    assert!(alerts.try_recv().is_err(), "the newer one is not tried");
    assert!(!held.join(&newer.file).exists(), "the newer one is not fetched");
    assert_eq!(read_manifest(&held).unwrap(), std::slice::from_ref(&kept), "the older one listed once");
    assert_eq!(std::fs::read(held.join(&kept.file)).unwrap(), std::fs::read(origin.join(&kept.file)).unwrap());
}

/// A snapshot an origin lists but cannot serve gives way to the next origin's copy at the same offset, which counts as
/// one attempt: a mirror that copied the manifest but not the files does not keep the operator's snapshot from the
/// member.
#[tokio::test]
async fn a_refused_origins_snapshot_gives_way_to_the_next_origins_at_its_offset() {
    let (dir, origin) = (scratch_dir("build-snapshot-mirror"), scratch_dir("build-snapshot-mirror-origin"));
    write_log(&dir).await;
    let older = publish_at(&dir, &origin, &recorded(), |held| held / 3);
    let newer = publish_at(&dir, &origin, &recorded(), |held| 2 * held / 3);
    assert!(older < newer);
    let mirror = scratch_dir("build-snapshot-mirror-files");
    std::fs::create_dir_all(&mirror).unwrap();
    std::fs::copy(origin.join(MANIFEST), mirror.join(MANIFEST)).unwrap();
    let member = scratch_dir("build-snapshot-mirror-member");
    let client = crate::standby::archive::archive_client(false).unwrap();
    let origins = [serve(&mirror).await, serve(&origin).await];
    let (_guard, mut alerts) = capture_alerts();
    let signed = snapshot_start(&client, &origins, (&member, &dir), DOMAIN, &[signer()].into(), (u64::MAX, None)).await;
    assert_eq!(signed.map(|signed| signed.snapshot.offset.seq), Some(newer), "the newest, from the operator");
    let alert = alerts.try_recv().expect("the mirror's refusal is paged");
    assert_eq!(
        (alert["alert"].as_str(), alert["seq"].as_str()),
        ("snapshot_start_refused", newer.to_string().as_str())
    );
    assert!(alerts.try_recv().is_err(), "the operator's copy passes");
    let kept: Vec<u64> = read_manifest(&member).unwrap().iter().map(|listing| listing.seq).collect();
    assert_eq!(kept, [newer], "the snapshot taken is listed");
}

/// A snapshot the member's directory lists but its host no longer holds, as one the archive's bucket keeps in its
/// place, is no candidate of the member's own and is not paged: the member goes on from the newest it holds, or
/// from an origin's copy of it.
#[tokio::test]
async fn a_listed_snapshot_the_host_no_longer_holds_is_passed_over_unpaged() {
    let (dir, origin) = (scratch_dir("build-snapshot-archived"), scratch_dir("build-snapshot-archived-origin"));
    write_log(&dir).await;
    let older = publish_at(&dir, &origin, &recorded(), |held| held / 3);
    let newer = publish_at(&dir, &origin, &recorded(), |held| 2 * held / 3);
    let held = scratch_dir("build-snapshot-archived-held");
    std::fs::create_dir_all(&held).unwrap();
    let listed = read_manifest(&origin).unwrap();
    std::fs::copy(origin.join(&listed[0].file), held.join(&listed[0].file)).unwrap();
    write_listings(&held, &listed);
    let (client, signers) = (crate::standby::archive::archive_client(false).unwrap(), [signer()].into());
    let (_guard, mut alerts) = capture_alerts();
    let signed = snapshot_start(&client, &[], (&held, &dir), DOMAIN, &signers, (u64::MAX, None)).await;
    assert_eq!(signed.map(|signed| signed.snapshot.offset.seq), Some(older), "the newest the host holds");
    assert!(alerts.try_recv().is_err(), "the one it no longer holds is not paged");
    let origins = [serve(&origin).await];
    let signed = snapshot_start(&client, &origins, (&held, &dir), DOMAIN, &signers, (u64::MAX, None)).await;
    assert_eq!(signed.map(|signed| signed.snapshot.offset.seq), Some(newer), "an origin's copy of it");
}
