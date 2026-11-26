//! A standby's cold start: a backfill a mirror served that does not replay as the takeover would is
//! set aside, paged and backfilled again from the operator, and the standby never follows onto it; one
//! the operator itself serves that does not replay is refused; a copy not yet marked checked (a crash
//! before the check, an earlier release's copy, or files an interrupted fetch left) is checked first;
//! and one that replays, or an archive that lists nothing, pages nothing.

use std::path::{Path, PathBuf};

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

use super::*;
use crate::log::{LogEntry, Manifest, SegmentInfo};
use crate::standby::archive::archive_client;
use crate::standby::tests::{DOMAIN, ticks};
use crate::tee::EnclaveKey;
use crate::test_support::{capture_alerts_and_resolutions, log_entries, scratch_dir};

/// A mirror's archive alters an early command's events, in a segment its own manifest lists under the
/// altered bytes, and leaves the last checkpoint alone, so the copy's chain at its end is the
/// operator's. The cold start pages Critical `standby_backfill_refused` naming the mirror, sets that
/// copy aside, and backfills again from the operator alone: the copy the standby then follows is the
/// operator's, replays as the takeover replays it, and the episode ends.
#[tokio::test]
async fn a_backfill_a_mirror_poisoned_is_set_aside_and_backfilled_from_the_operator() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let operator = scratch_dir("backfill-poisoned-operator");
    ticks(&operator, 9).await;
    let (honest, poisoned) =
        (archive_of(&operator, "backfill-poisoned-honest"), archive_of(&operator, "backfill-poisoned"));
    poison(&poisoned);
    let followed = followed(&serve(&operator).await, &serve(&poisoned).await);
    let dir = scratch_dir("backfill-poisoned-copy");

    start(&followed, &dir, signers()).await.unwrap();
    let paged = named(&mut alerts, "alert").expect("the refused backfill paged nothing");
    assert_eq!(paged["level"], "ERROR", "the refused backfill paged below Critical");
    assert!(paged["message"].contains(&followed.mirrors[0]), "the page does not name the mirror: {paged:?}");
    // The router keys an episode on its fields: the resolution ends the page's own, and the operator's.
    assert_eq!(served_by(&mut resolved, "alert_resolved"), ["archive", "operator"], "the episodes did not end");
    assert_eq!(log_entries(&dir), log_entries(&honest), "the standby follows onto another copy than the operator's");
    replay_as_takeover(&dir, (DOMAIN, Default::default()), signers(), None)
        .expect("the copy the standby follows onto does not replay");
    assert!(!needed(&dir), "the copy the standby follows onto is not marked checked");
    let aside = aside_of(&dir, "refused");
    assert_eq!(log_entries(&aside), log_entries(&poisoned), "the refused copy was not kept aside");
}

/// The operator's own archive serves a copy that does not replay either: the start fails, both refused
/// copies are kept aside, and no copy is left for a restart to follow onto.
#[tokio::test]
async fn a_backfill_the_operator_serves_that_does_not_replay_is_refused() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let operator = scratch_dir("backfill-refused-operator");
    ticks(&operator, 9).await;
    let poisoned = archive_of(&operator, "backfill-refused");
    poison(&poisoned);
    let served = serve(&poisoned).await;
    let dir = scratch_dir("backfill-refused-copy");

    let refused = start(&followed(&served, &served), &dir, signers()).await.expect_err("the standby took the copy");
    assert!(format!("{refused:#}").contains("the operator itself serves does not replay"), "{refused:#}");
    // The archive's refusal and the operator's own page apart, so the router's episodes keep both.
    let paged: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|line| line.get("alert").is_some_and(|name| name == "standby_backfill_refused"))
        .map(|line| line["served_by"].clone())
        .collect();
    assert_eq!(paged, ["archive", "operator"], "the refusals do not page apart");
    assert!(needed(&dir) && !dir.exists(), "a restart would follow onto a refused copy");
    for suffix in ["refused", "refused-operator"] {
        assert_eq!(log_entries(&aside_of(&dir, suffix)), log_entries(&poisoned), "the {suffix} copy was not kept");
    }
}

/// The check replays as the takeover does, against the signers it is given, not with no trust: an
/// honest archive whose every seal and checkpoint another key signed, which a replay with no trust
/// passes, is refused.
#[tokio::test]
async fn a_backfill_another_key_signed_is_refused_though_it_replays_with_no_trust() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let operator = scratch_dir("backfill-foreign-operator");
    ticks(&operator, 9).await;
    let archive = archive_of(&operator, "backfill-foreign-archive");
    crate::sequencer::replay_path(&archive).expect("the archive replays with no trust");
    let served = serve(&archive).await;
    let dir = scratch_dir("backfill-foreign-copy");
    let foreign = [AccountId(EnclaveKey::generate().address())].into();

    start(&followed(&served, &served), &dir, foreign).await.expect_err("a copy no accepted key signed was taken");
    assert!(named(&mut alerts, "alert").is_some(), "nothing paged");
}

/// A copy the standby has not marked checked is checked before it follows: one an earlier release
/// backfilled, or a crash left between the fetch and the check, and the files an interrupted fetch
/// linked before its manifest, which a mirror's empty manifest leaves as an open segment. A poisoned
/// one is set aside and backfilled again from the operator.
#[tokio::test]
async fn a_copy_not_marked_checked_is_checked_before_the_standby_follows() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let operator = scratch_dir("backfill-unchecked-operator");
    ticks(&operator, 9).await;
    let honest = archive_of(&operator, "backfill-unchecked-honest");
    let poisoned = archive_of(&operator, "backfill-unchecked-poisoned");
    poison(&poisoned);
    let empty = scratch_dir("backfill-unchecked-empty");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(empty.join(MANIFEST), "").unwrap();
    let segment = format!("{:020}.jsonl", 0);
    for case in ["listed", "linked before its manifest"] {
        let dir = scratch_dir(&format!("backfill-unchecked-{}", case.replace(' ', "-")));
        std::fs::create_dir_all(&dir).unwrap();
        let mirror = match case {
            "listed" => {
                crate::test_support::copy_dir(&poisoned, &dir);
                serve(&poisoned).await
            }
            _ => {
                std::fs::copy(poisoned.join(&segment), dir.join(&segment)).unwrap();
                serve(&empty).await
            }
        };
        assert!(needed(&dir), "{case}: a copy not marked checked is not checked");
        start(&followed(&serve(&operator).await, &mirror), &dir, signers())
            .await
            .unwrap_or_else(|e| panic!("{case}: {e:#}"));
        assert!(named(&mut alerts, "alert").is_some(), "{case}: nothing paged");
        assert_eq!(log_entries(&dir), log_entries(&honest), "{case}: the standby follows onto the poisoned copy");
    }
}

/// A backfill that replays, and an archive that lists nothing yet, as a fresh deployment's, page
/// nothing and leave the copy marked checked.
#[tokio::test]
async fn a_backfill_that_replays_or_lists_nothing_pages_nothing() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let operator = scratch_dir("backfill-clean-operator");
    ticks(&operator, 9).await;
    let empty = scratch_dir("backfill-clean-empty");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(empty.join(MANIFEST), "").unwrap();
    for (case, archive) in [("replays", &operator), ("lists nothing", &empty)] {
        let followed = followed(&serve(archive).await, &serve(archive).await);
        let dir = scratch_dir(&format!("backfill-clean-{}", case.replace(' ', "-")));
        start(&followed, &dir, signers()).await.unwrap_or_else(|e| panic!("{case}: {e:#}"));
        assert!(named(&mut alerts, "alert").is_none(), "{case}: a backfill paged");
        assert!(!needed(&dir), "{case}: the copy is not marked checked");
    }
}

/// A takeover's cut a crash interrupted after its marker is finished before the cold start checks the
/// copy, and its marker removed: the copy checked is the one followed, which replays only once the cut has
/// dropped a segment past the marker's checkpoint that a mirror altered.
#[tokio::test]
async fn a_takeover_cut_a_crash_interrupted_is_finished_before_the_check() {
    let operator = scratch_dir("backfill-cut-operator");
    ticks(&operator, 9).await;
    let served = serve(&operator).await;
    let dir = archive_of(&operator, "backfill-cut-copy");
    let first = log_entries(&dir)
        .into_iter()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .expect("the copy holds a checkpoint");
    let signer = AccountId(EnclaveKey::test_key().address());
    let marker = crate::checkpoints::Takeover { seq: first.seq, root: first.root.clone(), signer };
    std::fs::write(dir.join("takeover.json"), serde_json::to_vec(&marker).unwrap()).unwrap();
    let past = Manifest::load(&dir).unwrap().segments.last().cloned().unwrap();
    assert!(past.first_seq > first.seq, "the last segment holds the marker's checkpoint");
    poison_segment(&dir, &past.file);
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    // A marker a crash cut short is only ever its staging file, which nothing reads.
    std::fs::write(dir.join("takeover.json.staging"), "{\"seq\":").unwrap();
    start(&followed(&served, &served), &dir, signers()).await.unwrap();
    assert!(!dir.join("takeover.json").exists(), "the marker outlived its cut");
    assert!(
        matches!(log_entries(&dir).last(), Some(LogEntry::Checkpoint(checkpoint)) if checkpoint.seq == first.seq),
        "the cut was not finished"
    );
    assert!(!needed(&dir), "the cut copy was not checked");
    assert!(named(&mut alerts, "alert").is_none(), "the copy was checked before its cut was finished");
}

/// A copy that holds commands but no checkpoint, as a mirror that cut it short of one serves, or a copy an
/// earlier release followed from seq 0 before the first checkpoint, cannot be checked: it is set aside and
/// backfilled again from the operator, with no page. One the operator itself serves with no checkpoint yet
/// is set aside unmarked, so the stream refills it from seq 0.
#[tokio::test]
async fn a_copy_with_no_checkpoint_yet_is_backfilled_from_the_operator_or_left_to_the_stream() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let operator = scratch_dir("backfill-nocheckpoint-operator");
    ticks(&operator, 9).await;
    let young = scratch_dir("backfill-nocheckpoint-young");
    std::fs::create_dir_all(&young).unwrap();
    let segment = format!("{:020}.jsonl", 0);
    let lines: Vec<u8> = std::fs::read(operator.join(&segment))
        .unwrap()
        .split_inclusive(|byte| *byte == b'\n')
        .take_while(|line| !matches!(serde_json::from_slice(line), Ok(LogEntry::Checkpoint(_))))
        .flatten()
        .copied()
        .collect();
    assert!(!lines.is_empty(), "the log starts with commands before its first checkpoint");
    let last_seq = lines
        .split_inclusive(|byte| *byte == b'\n')
        .filter_map(|line| match serde_json::from_slice(line) {
            Ok(LogEntry::Command(command)) => Some(command.sequenced.seq),
            _ => None,
        })
        .next_back()
        .unwrap();
    // Listed as a completed segment, as an archive whose first segment ends before any checkpoint lists it.
    let listed = SegmentInfo {
        file: segment.clone(),
        first_seq: 0,
        last_seq,
        bytes: lines.len() as u64,
        sha256: hex::encode(Sha256::digest(&lines)),
    };
    std::fs::write(young.join(&segment), &lines).unwrap();
    std::fs::write(young.join(MANIFEST), serde_json::to_string(&listed).unwrap() + "\n").unwrap();
    let (mature, young_log) = (serve(&operator).await, serve(&young).await);
    for (case, origin) in [("the operator holds checkpoints", &mature), ("the operator holds none yet", &young_log)] {
        let dir = scratch_dir(&format!("backfill-nocheckpoint-{}", case.replace(' ', "-")));
        crate::test_support::copy_dir(&young, &dir);
        start(&followed(origin, &young_log), &dir, signers()).await.unwrap_or_else(|e| panic!("{case}: {e:#}"));
        assert!(named(&mut alerts, "alert").is_none(), "{case}: a copy with nothing to check yet paged");
        assert!(aside_of(&dir, "unchecked").exists(), "{case}: the uncheckable copy was not set aside");
        if origin == &mature {
            assert!(!needed(&dir), "{case}: the operator's copy was not marked checked");
            let archived = log_entries(&archive_of(&operator, "backfill-nocheckpoint-archived"));
            assert_eq!(log_entries(&dir), archived, "{case}: the copy is not the operator's archive");
        } else {
            assert!(!dir.exists() && needed(&dir), "{case}: the copy was not left to the stream");
        }
        // Whether the copy is checked or left to the stream, both refusal episodes end.
        let ended = served_by(&mut resolved, "alert_resolved");
        assert_eq!(ended, ["archive", "operator"], "{case}: a refusal episode is left open");
    }
}

/// Once a copy is set aside as refused, a start whose copy lists nothing, as after a rebuild from the
/// operator that did not finish, backfills from the operator alone: the refused mirror's copy is not taken
/// again, to be refused and kept from its set-aside by the earlier one. A copy the operator serves that
/// does not replay then is the operator's: its page names the operator alone, and it is set aside to
/// `<dir>.refused-operator`.
#[tokio::test]
async fn a_start_after_a_refusal_backfills_from_the_operator_alone() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let operator = scratch_dir("backfill-after-refusal-operator");
    ticks(&operator, 9).await;
    let poisoned = archive_of(&operator, "backfill-after-refusal-poisoned");
    poison(&poisoned);
    let dir = scratch_dir("backfill-after-refusal-copy");
    std::fs::create_dir_all(aside_of(&dir, "refused")).unwrap();
    start(&followed(&serve(&operator).await, &serve(&poisoned).await), &dir, signers()).await.unwrap();
    assert!(named(&mut alerts, "alert").is_none(), "the refused mirror's copy was taken again");
    let archived = log_entries(&archive_of(&operator, "backfill-after-refusal-archived"));
    assert_eq!(log_entries(&dir), archived, "the copy is not the operator's");

    let dir = scratch_dir("backfill-after-refusal-refused");
    std::fs::create_dir_all(aside_of(&dir, "refused")).unwrap();
    let (origin, mirror) = (serve(&poisoned).await, serve(&operator).await);
    let failed = start(&followed(&origin, &mirror), &dir, signers()).await.expect_err("the start took the copy");
    assert!(format!("{failed:#}").contains("the operator itself serves does not replay"), "{failed:#}");
    let paged = named(&mut alerts, "alert").expect("the operator's copy paged nothing");
    assert!(paged["message"].contains(&origin) && !paged["message"].contains(&mirror), "{paged:?}");
    assert!(
        aside_of(&dir, "refused-operator").exists() && !dir.exists(),
        "the copy was not set aside as the operator's"
    );
}

/// A refused copy pages before it is set aside, so a set-aside that fails, as onto a path a file holds,
/// still pages, and its error keeps the refusal.
#[tokio::test]
async fn a_refusal_pages_before_the_copy_is_set_aside() {
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let operator = scratch_dir("backfill-aside-operator");
    ticks(&operator, 9).await;
    let dir = archive_of(&operator, "backfill-aside-copy");
    poison(&dir);
    let blocked = aside_of(&dir, "refused");
    std::fs::write(&blocked, "a file where the set-aside copy goes").unwrap();
    let served = serve(&operator).await;

    let failed = start(&followed(&served, &served), &dir, signers()).await.expect_err("the copy was set aside");
    std::fs::remove_file(&blocked).unwrap();
    assert!(named(&mut alerts, "alert").is_some(), "the refusal did not page");
    assert!(format!("{failed:#}").contains("after refusing the copy"), "{failed:#}");
}

/// Every start whose copy is marked checked reasserts the resolution, so one a crash lost after the mark
/// still ends the episode; an unmarked copy resolves nothing.
#[test]
fn a_checked_copy_resolves_the_episode_on_every_start() {
    let (_guard, _alerts, mut resolved) = capture_alerts_and_resolutions();
    let dir = scratch_dir("backfill-resolve");
    std::fs::create_dir_all(&dir).unwrap();
    resolve(&dir);
    assert!(named(&mut resolved, "alert_resolved").is_none(), "an unchecked copy resolved the episode");
    std::fs::write(dir.join(CHECKED), "").unwrap();
    resolve(&dir);
    let ended = served_by(&mut resolved, "alert_resolved");
    assert_eq!(ended, ["archive", "operator"], "a checked copy did not resolve both episodes");
}

/// A copy is set aside beside its directory however the directory is written.
#[test]
fn a_copy_is_set_aside_beside_its_directory() {
    for (dir, aside) in
        [("/data/log", "/data/log.refused"), ("/data/log/", "/data/log.refused"), ("log", "log.refused")]
    {
        assert_eq!(aside_of(Path::new(dir), "refused"), PathBuf::from(aside), "{dir}");
    }
}

/// A copy an earlier start set aside as refused is evidence, and may be the standby's own: a later
/// refusal leaves both where they are rather than delete either. A copy nothing could check is replaced.
#[test]
fn a_refused_copy_set_aside_earlier_is_never_replaced() {
    for (suffix, replaced) in [("refused", false), ("refused-operator", false), ("unchecked", true)] {
        let dir = scratch_dir(&format!("backfill-aside-kept-{suffix}"));
        for held in ["the earlier copy", "the later copy"] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("held"), held).unwrap();
            let aside = set_aside(&dir, suffix);
            match (held, replaced, aside) {
                ("the earlier copy", _, aside) => assert!(aside.is_ok(), "{suffix}: the first set-aside failed"),
                (_, true, aside) => assert!(aside.is_ok(), "{suffix}: the later copy was not set aside"),
                (_, false, aside) => assert!(aside.is_err(), "{suffix}: the later copy replaced the earlier one"),
            }
        }
        let kept = std::fs::read_to_string(aside_of(&dir, suffix).join("held")).unwrap();
        let expected = if replaced { "the later copy" } else { "the earlier copy" };
        assert_eq!(kept, expected, "{suffix}: the copy set aside");
        assert_eq!(dir.exists(), !replaced, "{suffix}: the later copy was removed");
    }
}

mod refresh;
mod snapshot;

// ── Helpers ──

/// What `serve` runs at a cold start: the fetch, then the check of the whole copy against `signers`.
async fn start(followed: &Followed, dir: &Path, signers: BTreeSet<AccountId>) -> anyhow::Result<()> {
    let trust = async || Ok(Trust { signers: signers.clone(), start: None });
    prepare(&archive_client(false).unwrap(), followed, dir, (DOMAIN, Default::default()), trust).await
}

/// The first line `lines` holds whose `key` names `standby_backfill_refused`.
fn named(
    lines: &mut tokio::sync::mpsc::UnboundedReceiver<crate::test_support::Alert>,
    key: &str,
) -> Option<crate::test_support::Alert> {
    std::iter::from_fn(|| lines.try_recv().ok())
        .find(|line| line.get(key).is_some_and(|name| name == "standby_backfill_refused"))
}

/// The `served_by` of each line `lines` holds whose `key` names `standby_backfill_refused`, in order: the
/// episode each pages or resolves, as the alert router keys it with its other fields.
fn served_by(lines: &mut tokio::sync::mpsc::UnboundedReceiver<crate::test_support::Alert>, key: &str) -> Vec<String> {
    std::iter::from_fn(|| lines.try_recv().ok())
        .filter(|line| line.get(key).is_some_and(|name| name == "standby_backfill_refused"))
        .map(|line| line.get("served_by").cloned().unwrap_or_default())
        .collect()
}

/// The test sequencer's enclave, the only signer of its log.
fn signers() -> BTreeSet<AccountId> {
    [AccountId(EnclaveKey::test_key().address())].into()
}

/// The operator at `origin`, with `mirror` its only archive mirror.
fn followed(origin: &str, mirror: &str) -> Followed {
    Followed {
        origin: origin.to_owned(),
        enclave: AccountId(EnclaveKey::test_key().address()),
        mirrors: vec![mirror.to_owned()],
        danger_accept_invalid_certs: false,
    }
}

/// What an archive of the log in `operator` serves, in a new directory named for `name`: its completed
/// segments and their listings.
fn archive_of(operator: &Path, name: &str) -> PathBuf {
    let dir = scratch_dir(name);
    std::fs::create_dir_all(&dir).unwrap();
    let listings = Manifest::load(operator).unwrap().segments;
    assert!(listings.len() >= 2, "the archive lists at least two completed segments");
    for listed in listings.iter() {
        std::fs::copy(operator.join(&listed.file), dir.join(&listed.file)).unwrap();
    }
    std::fs::copy(operator.join(MANIFEST), dir.join(MANIFEST)).unwrap();
    dir
}

/// Adds an event to the log's first command in `dir`, and lists its segment under the altered bytes,
/// as a mirror serving its own manifest does.
fn poison(dir: &Path) {
    poison_segment(dir, &format!("{:020}.jsonl", 0));
}

/// `poison` on the first command of the segment `file` in `dir`.
fn poison_segment(dir: &Path, file: &str) {
    alter_segment(dir, file, |n, entry| {
        if let (0, LogEntry::Command(command)) = (n, entry) {
            let rejected = engine_core::event::Event::AdminRejected { reason: "poisoned".into() };
            command.events.get_or_insert_with(Vec::new).push(rejected);
        }
    });
}

/// Rewrites each entry of the segment `file` in `dir` through `edit`, given its line, and lists the
/// segment under the altered bytes, as a mirror serving its own manifest does.
fn alter_segment(dir: &Path, file: &str, mut edit: impl FnMut(usize, &mut LogEntry)) {
    let mut bytes = Vec::new();
    for (n, line) in std::fs::read(dir.join(file)).unwrap().split_inclusive(|byte| *byte == b'\n').enumerate() {
        let mut entry = serde_json::from_slice::<LogEntry>(line).unwrap();
        edit(n, &mut entry);
        bytes.extend(serde_json::to_vec(&entry).unwrap());
        bytes.push(b'\n');
    }
    std::fs::write(dir.join(file), &bytes).unwrap();
    let listings: Vec<SegmentInfo> = Manifest::load(dir).unwrap().segments.iter().cloned().collect();
    let text: String = listings
        .into_iter()
        .map(|mut listed| {
            if listed.file == file {
                (listed.bytes, listed.sha256) = (bytes.len() as u64, hex::encode(Sha256::digest(&bytes)));
            }
            serde_json::to_string(&listed).unwrap() + "\n"
        })
        .collect();
    std::fs::write(dir.join(MANIFEST), text).unwrap();
}

/// Serves the files in `dir` as an archive, over plain HTTP on loopback: its base URL.
async fn serve(dir: &Path) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = dir.to_owned();
    let app = Router::new().route(
        "/v1/archive/{file}",
        get(move |axum::extract::Path(file): axum::extract::Path<String>| {
            let path = dir.join(&file);
            async move {
                match std::fs::read(path) {
                    Ok(bytes) => bytes.into_response(),
                    Err(_) => StatusCode::NOT_FOUND.into_response(),
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}
