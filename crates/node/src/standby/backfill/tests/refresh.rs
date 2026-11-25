//! A takeover's archive refresh: a refresh its replay refuses, or a crash interrupts, is dropped, and the
//! copy left marked as it was.

use super::*;

/// A takeover that fails leaves the copy marked as it was, whether its replay passed and its anchor is
/// missing, or its replay refused the copy, and forgets its refresh record.
#[tokio::test]
async fn a_failed_takeover_leaves_the_copy_marked_as_it_was() {
    let operator = scratch_dir("backfill-takeover-operator");
    ticks(&operator, 9).await;
    let served = serve(&operator).await;
    let signer = AccountId(EnclaveKey::test_key().address());
    let unheld = crate::checkpoints::Takeover { seq: u64::MAX, root: "0x00".into(), signer };
    // No archive answers: the refresh adds nothing.
    let origins = ["http://127.0.0.1:9".to_owned()];
    for (case, signers) in [("replays", signers()), ("does not replay", [AccountId([9; 20])].into())] {
        let dir = scratch_dir(&format!("backfill-takeover-{}", case.replace(' ', "-")));
        start(&followed(&served, &served), &dir, self::signers()).await.unwrap();
        let copied = crate::standby::chain::Chain::new(DOMAIN, signer);
        crate::standby::finalize(
            &archive_client(false).unwrap(),
            (DOMAIN, signers, Default::default()),
            &origins,
            (&dir, None),
            &unheld,
            copied,
        )
        .await
        .expect_err("a takeover without its anchor");
        assert!(!needed(&dir), "{case}: the copy is no longer marked");
        assert!(!dir.join(REFRESH).exists(), "{case}: the refresh record outlived the takeover");
    }
}

/// A refresh the takeover's replay never committed is dropped: when the replay refuses what the refresh
/// added, after which the copy the stream gave replays and is marked checked, and at the next start after a
/// crash before the replay ended, which marks the copy checked again if it was. A restart then follows what
/// the stream gave rather than refusing it. A segment the copy held unlisted before the refresh, as the
/// stream's open one, is kept.
#[tokio::test]
async fn a_refresh_the_takeover_never_committed_is_dropped() {
    let operator = scratch_dir("backfill-refresh-operator");
    ticks(&operator, 9).await;
    let served = serve(&operator).await;
    for (case, checked, crashed) in
        [("refused marked", true, false), ("refused", false, false), ("crashed", true, true)]
    {
        let (dir, before, last, bytes) =
            short_of_last(&operator, &format!("backfill-refresh-{}", case.replace(' ', "-")));
        if checked {
            std::fs::write(dir.join(CHECKED), "").unwrap();
        }
        begin_refresh(&dir).unwrap();
        assert!(needed(&dir), "{case}: the refresh left the copy marked");
        // The refresh links the last segment, which a mirror altered.
        std::fs::write(dir.join(&last.file), &bytes).unwrap();
        std::fs::write(dir.join(MANIFEST), format!("{before}{}\n", serde_json::to_string(&last).unwrap())).unwrap();
        poison_segment(&dir, &last.file);
        match crashed {
            true => start(&followed(&served, &served), &dir, signers()).await.unwrap(),
            false => replay_refreshed(&dir, (DOMAIN, Default::default()), signers(), None)
                .unwrap_or_else(|e| panic!("{case}: the copy the stream gave does not replay: {e:#}")),
        }
        assert_eq!(std::fs::read_to_string(dir.join(MANIFEST)).unwrap(), before, "{case}: the copy's listings");
        assert!(!dir.join(&last.file).exists(), "{case}: the refreshed segment is still there");
        assert!(!dir.join(REFRESH).exists(), "{case}: the refresh record outlived it");
        assert_eq!(!needed(&dir), checked || !crashed, "{case}: the copy's mark");
    }
    let (dir, before, last, bytes) = short_of_last(&operator, "backfill-refresh-open");
    std::fs::write(dir.join(&last.file), &bytes).unwrap();
    begin_refresh(&dir).unwrap();
    assert_eq!(undo_refresh(&dir).unwrap(), Some(0), "a refresh that added nothing dropped something");
    assert!(dir.join(&last.file).exists(), "the stream's open segment was dropped");
    assert_eq!(std::fs::read_to_string(dir.join(MANIFEST)).unwrap(), before);
}

/// A takeover's replay that refuses its refresh, when dropping what the refresh added then fails, keeps
/// the refusal in its error beside the failure, and keeps the refresh record for the next start to drop.
#[tokio::test]
async fn a_refresh_whose_drop_fails_keeps_the_refusal_and_its_record() {
    let operator = scratch_dir("backfill-refresh-undo-operator");
    ticks(&operator, 9).await;
    let (dir, _, last, bytes) = short_of_last(&operator, "backfill-refresh-undo");
    begin_refresh(&dir).unwrap();
    std::fs::write(dir.join(&last.file), &bytes).unwrap();
    // A directory under a segment's name, which no removal of a file takes away.
    let stuck = dir.join(format!("{:020}.jsonl", last.last_seq + 1_000));
    std::fs::create_dir_all(stuck.join("held")).unwrap();
    let refused =
        replay_refreshed(&dir, (DOMAIN, Default::default()), signers(), None).expect_err("the refreshed copy replayed");
    let message = format!("{refused:#}");
    // The refusal, the replay's own: the segment the refresh added does not open beside the stuck one.
    assert!(message.contains("is followed by another segment"), "the refusal was lost: {message}");
    // And the failure to drop it, with its cause.
    assert!(message.contains("dropping what the archive refresh added failed"), "{message}");
    assert!(message.contains(&format!("removing {}", stuck.file_name().unwrap().to_string_lossy())), "{message}");
    assert!(message.contains("the next start drops it"), "{message}");
    assert!(dir.join(REFRESH).exists(), "the refresh record went before its drop");
}

/// A takeover whose archive refresh links a segment a mirror altered so the copy's own chain admits it, a
/// checkpoint signed by a key the vault never recorded, drops it once its replay refuses it, and the copy
/// stays marked: the refresh is taken from the archive, before it, and undone end to end. Here the copy
/// the stream gave lacks the anchor, so the takeover then fails on that.
#[tokio::test]
async fn a_takeover_drops_a_refreshed_segment_its_replay_refuses() {
    let operator = scratch_dir("backfill-takeover-refresh-operator");
    ticks(&operator, 9).await;
    let mirror = archive_of(&operator, "backfill-takeover-refresh-mirror");
    let altered = Manifest::load(&mirror).unwrap().segments.last().cloned().unwrap();
    resign_checkpoints(&mirror, &altered.file, &EnclaveKey::generate());
    let (dir, before, _, _) = short_of_last(&operator, "backfill-takeover-refresh-copy");
    std::fs::write(dir.join(CHECKED), "").unwrap();
    let signer = AccountId(EnclaveKey::test_key().address());
    let (writer, copied) =
        crate::standby::open_copy(&dir, 4, crate::standby::chain::Chain::new(DOMAIN, signer)).unwrap();
    drop(writer);
    let unheld = crate::checkpoints::Takeover { seq: u64::MAX, root: "0x00".into(), signer };
    let origins = [serve(&mirror).await];
    let refused = crate::standby::finalize(
        &archive_client(false).unwrap(),
        (DOMAIN, signers(), Default::default()),
        &origins,
        (&dir, None),
        &unheld,
        copied,
    )
    .await
    .expect_err("the takeover took a refreshed segment no replay admits, or a copy without its anchor");
    assert!(format!("{refused:#}").contains("no checkpoint at"), "{refused:#}");
    assert_eq!(std::fs::read_to_string(dir.join(MANIFEST)).unwrap(), before, "the copy's listings");
    assert!(!dir.join(&altered.file).exists(), "the refreshed segment is still there");
    assert!(!needed(&dir), "the stream's copy is no longer marked");
}

/// A copy that holds the vault's anchor takes over as the stream gave it when a mirror listed before the
/// archive serves the copy's next segment with its checkpoints signed by another key, at every attempt,
/// as it does when no origin answers.
#[tokio::test]
async fn a_takeover_refused_a_refreshed_segment_goes_on_from_the_copy_the_stream_gave() {
    let operator = scratch_dir("backfill-takeover-fallback-operator");
    ticks(&operator, 9).await;
    let mirror = archive_of(&operator, "backfill-takeover-fallback-mirror");
    let altered = Manifest::load(&mirror).unwrap().segments.last().cloned().unwrap();
    resign_checkpoints(&mirror, &altered.file, &EnclaveKey::generate());
    let honest = archive_of(&operator, "backfill-takeover-fallback-honest");
    let silent = vec!["http://127.0.0.1:9".to_owned()];
    let poisoned = vec![serve(&mirror).await, serve(&honest).await];
    for (case, origins) in [("no origin answers", &silent), ("a mirror is first", &poisoned)] {
        let (dir, _, _, _) =
            short_of_last(&operator, &format!("backfill-takeover-fallback-{}", case.replace(' ', "-")));
        std::fs::write(dir.join(CHECKED), "").unwrap();
        let anchor = log_entries(&dir)
            .into_iter()
            .rev()
            .find_map(|entry| match entry {
                LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
                _ => None,
            })
            .expect("the copy holds a checkpoint");
        let signer = AccountId(EnclaveKey::test_key().address());
        let takeover = crate::checkpoints::Takeover { seq: anchor.seq, root: anchor.root.clone(), signer };
        let (writer, copied) =
            crate::standby::open_copy(&dir, 4, crate::standby::chain::Chain::new(DOMAIN, signer)).unwrap();
        drop(writer);
        crate::standby::finalize(
            &archive_client(false).unwrap(),
            (DOMAIN, signers(), Default::default()),
            origins,
            (&dir, None),
            &takeover,
            copied,
        )
        .await
        .unwrap_or_else(|e| panic!("{case}: the copy the stream gave takes over: {e:#}"));
        assert!(!dir.join(&altered.file).exists(), "{case}: a refused segment was kept");
    }
}

// ── Helpers ──

/// Signs every checkpoint of the segment `file` in `dir` with `key` instead, over the same fields: a
/// copy's chain admits it, as it admits any checkpoint signed by the key it names, and no replay does.
pub(super) fn resign_checkpoints(dir: &Path, file: &str, key: &EnclaveKey) {
    alter_segment(dir, file, |_, entry| {
        if let LogEntry::Checkpoint(checkpoint) = entry {
            let domain = checkpoint.domain.unwrap_or(DOMAIN);
            let root: [u8; 32] = hex::decode(checkpoint.root.trim_start_matches("0x")).unwrap().try_into().unwrap();
            checkpoint.signature = match checkpoint.parent {
                Some(parent) => key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, checkpoint.seq, &root, parent),
                None => key.sign_checkpoint(domain.chain_id, &domain.vault.0, checkpoint.seq, &root),
            };
            checkpoint.signer = key.address_hex();
        }
    });
}

/// The copy of the operator's archive at `name`, without its last listed segment, as before a takeover's
/// refresh: the manifest it then lists, and the listing of the segment left out, whose bytes it returns.
pub(super) fn short_of_last(operator: &Path, name: &str) -> (PathBuf, String, SegmentInfo, Vec<u8>) {
    let dir = archive_of(operator, name);
    let manifest = std::fs::read_to_string(dir.join(MANIFEST)).unwrap();
    let mut lines: Vec<&str> = manifest.lines().collect();
    let last: SegmentInfo = serde_json::from_str(lines.pop().unwrap()).unwrap();
    let before = lines.iter().map(|line| format!("{line}\n")).collect::<String>();
    let bytes = std::fs::read(dir.join(&last.file)).unwrap();
    std::fs::remove_file(dir.join(&last.file)).unwrap();
    std::fs::write(dir.join(MANIFEST), &before).unwrap();
    (dir, before, last, bytes)
}
