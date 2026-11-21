//! A follower's replica built from a daily snapshot, against one built by a whole replay of its copy.

use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::daily_snapshots::{Job, Listed, MANIFEST, Signing, publish_now, read_manifest, snapshot_start};
use crate::log_check::answer::CheckedHeads;
use crate::snapshot::sealing::Encoding;
use crate::tee::EnclaveKey;
use crate::test_support::capture_alerts;
use crate::test_support::{SEGMENT_COMMANDS, log_entries, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// A follower whose copy holds a signed daily snapshot's checkpoint builds its replica from the snapshot, with no
/// whole replay, to the state, command prefix and last checkpoint a whole replay gives, and starts its turn from
/// it as from any replica.
#[tokio::test]
async fn a_follower_builds_its_replica_from_a_signed_snapshot() {
    let (dir, snapshots) = (scratch_dir("build-snapshot-copy"), scratch_dir("build-snapshot-daily"));
    write_log(&dir).await;
    let seq = publish_middle(&dir, &snapshots, &recorded());
    let (_guard, mut alerts) = capture_alerts();
    let signed = usable(&snapshots, &dir, &[signer()].into(), u64::MAX).await.expect("a usable snapshot");
    assert_eq!(signed.snapshot.offset.seq, seq);
    let from = open(&dir, Some(signed));
    let whole = open(&dir, None);
    assert_eq!((from.replays(), whole.replays()), (0, 1), "the snapshot's build replays nothing whole");
    assert_eq!(from.replica().head_link(), whole.replica().head_link());
    assert_eq!(from.replica().last_checkpoint(), whole.replica().last_checkpoint());
    assert_eq!(engine_json(&from), engine_json(&whole));
    assert!(alerts.try_recv().is_err(), "a usable snapshot pages nothing");
    drop(whole);
    from.into_log(None, signer(), None).expect("the replica from the snapshot starts its turn");
}

/// A snapshot whose state its signer vouched for but whose root is not its checkpoint's is refused when the
/// replica applies the checkpoint, paged as Warning `snapshot_start_refused`, and the replica replays the whole
/// copy instead.
#[tokio::test]
async fn a_snapshot_whose_root_is_not_its_checkpoints_falls_back_to_a_whole_replay() {
    let (dir, snapshots) = (scratch_dir("build-snapshot-root"), scratch_dir("build-snapshot-root-daily"));
    write_log(&dir).await;
    let signing = Signing { key: Arc::new(EnclaveKey::test_key()), domain: DOMAIN };
    publish_middle(&dir, &snapshots, &signing);
    // The signer vouches for a balance the checkpoint's root does not hold.
    let listing = read_manifest(&snapshots).unwrap().pop().unwrap();
    let path = snapshots.join(&listing.file);
    let mut written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let accounts = written["engine"]["accounts"].as_object_mut().unwrap();
    accounts.values_mut().next().unwrap()["balance"] = serde_json::json!("1");
    let checkpoint: Checkpoint = serde_json::from_value(written["checkpoint"].clone()).unwrap();
    let engine: Engine = serde_json::from_value(written["engine"].take()).unwrap();
    let engine = Encoding::Encoded(serde_json::to_string(&engine).unwrap());
    std::fs::remove_file(snapshots.join(MANIFEST)).unwrap();
    let offset = Offset { seq: listing.seq, index: listing.index };
    publish_now(&snapshots, Job { offset, checkpoint, ts: listing.ts, engine, signing }).unwrap();
    let (_guard, mut alerts) = capture_alerts();
    let signed = usable(&snapshots, &dir, &[signer()].into(), u64::MAX).await.expect("signed over its state");
    let follower = open(&dir, Some(signed));
    assert_eq!(follower.replays(), 1, "the replica replayed the whole copy");
    assert_eq!(engine_json(&follower), engine_json(&open(&dir, None)));
    let alert = alerts.try_recv().expect("the refusal is paged");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("snapshot_start_refused", "WARN"));
    assert!(alert["message"].contains("state root mismatch"), "{alert:?}");
}

/// A snapshot no replica starts from is passed over, paged as Warning `snapshot_start_refused`: one whose state
/// is not the state its signature covers, one signed by a key the vault has not recorded, one of the format
/// before signatures, and one of a checkpoint the copy does not hold. One of a checkpoint past the bound, the
/// vault's finalized one, is not tried.
#[tokio::test]
async fn a_snapshot_a_replica_cannot_start_from_is_passed_over() {
    let dir = scratch_dir("build-snapshot-refused");
    write_log(&dir).await;
    let recorded = Signing { key: Arc::new(EnclaveKey::test_key()), domain: DOMAIN };
    type Alter = fn(&mut serde_json::Value);
    let cases: [(&str, Option<Alter>, Signing, &str); 4] = [
        (
            "an altered state",
            Some(|snapshot| snapshot["engine"]["accounts"].as_object_mut().unwrap().clear()),
            recorded.clone(),
            "is not the state its signature covers",
        ),
        (
            "an unrecorded signer",
            None,
            Signing { key: Arc::new(EnclaveKey::generate()), domain: DOMAIN },
            "is not signed by",
        ),
        (
            "the unsigned format",
            Some(|snapshot| {
                snapshot["schema"] = 1.into();
                let object = snapshot.as_object_mut().unwrap();
                object.remove("state");
                object.remove("signature");
            }),
            recorded.clone(),
            "is not one of the signed format",
        ),
        (
            "a checkpoint the copy does not hold",
            Some(|snapshot| snapshot["checkpoint"]["root"] = serde_json::json!(format!("0x{}", hex::encode([3; 32])))),
            recorded.clone(),
            "the copy holds no checkpoint",
        ),
    ];
    for (case, alter, signing, said) in cases {
        let snapshots = scratch_dir("build-snapshot-refused-daily");
        let seq = publish_middle(&dir, &snapshots, &signing);
        let listing = read_manifest(&snapshots).unwrap().pop().unwrap();
        if let Some(alter) = alter {
            let path = snapshots.join(&listing.file);
            let mut written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            alter(&mut written);
            std::fs::write(&path, serde_json::to_vec(&written).unwrap()).unwrap();
            if case == "a checkpoint the copy does not hold" {
                rewrite_listing(&snapshots, |listing| {
                    listing.root = written["checkpoint"]["root"].as_str().unwrap().into()
                });
            }
        }
        let (guard, mut alerts) = capture_alerts();
        assert!(usable(&snapshots, &dir, &[signer()].into(), u64::MAX).await.is_none(), "{case}");
        let alert = alerts.try_recv().unwrap_or_else(|_| panic!("{case}: nothing paged"));
        assert_eq!(alert["alert"].as_str(), "snapshot_start_refused", "{case}");
        assert_eq!(alert["seq"], seq.to_string(), "{case}");
        assert!(alert["message"].contains(said), "{case}: {alert:?}");
        drop(guard);
        let (_guard, mut alerts) = capture_alerts();
        assert!(usable(&snapshots, &dir, &[signer()].into(), seq - 1).await.is_none(), "{case}: past the bound");
        assert!(alerts.try_recv().is_err(), "{case}: a snapshot past the bound is not tried");
    }
    // Signed by its checkpoint's signer, which the vault has not recorded.
    let snapshots = scratch_dir("build-snapshot-refused-daily");
    publish_middle(&dir, &snapshots, &recorded);
    let (_guard, mut alerts) = capture_alerts();
    let other = AccountId(EnclaveKey::generate().address());
    assert!(usable(&snapshots, &dir, &[other].into(), u64::MAX).await.is_none());
    let alert = alerts.try_recv().expect("the refusal is paged");
    assert!(alert["message"].contains("which the vault has not recorded"), "{alert:?}");
}

/// A snapshot whose embedded checkpoint names the copy's root but another recorded signer, which signed the
/// snapshot, is not taken: the copy holds another checkpoint at its offset, which the start finds, and the
/// replica's build finds too should one reach it, each paged as Warning `snapshot_start_refused`, the build before
/// the whole replay.
#[tokio::test]
async fn a_snapshot_of_another_signers_checkpoint_falls_back_to_a_whole_replay() {
    let (dir, snapshots) = (scratch_dir("build-snapshot-signer"), scratch_dir("build-snapshot-signer-daily"));
    write_log(&dir).await;
    publish_middle(&dir, &snapshots, &recorded());
    let other = Signing { key: Arc::new(EnclaveKey::generate()), domain: DOMAIN };
    let listing = read_manifest(&snapshots).unwrap().pop().unwrap();
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(snapshots.join(&listing.file)).unwrap()).unwrap();
    let mut checkpoint: Checkpoint = serde_json::from_value(written["checkpoint"].clone()).unwrap();
    let root: B256 = checkpoint.root.parse().unwrap();
    checkpoint.signature = other.key.sign_checkpoint(DOMAIN.chain_id, &DOMAIN.vault.0, checkpoint.seq, &root.0);
    checkpoint.signer = other.key.address_hex();
    let engine: Engine = serde_json::from_value(written["engine"].clone()).unwrap();
    let engine = Encoding::Encoded(serde_json::to_string(&engine).unwrap());
    std::fs::remove_file(snapshots.join(MANIFEST)).unwrap();
    let offset = Offset { seq: listing.seq, index: listing.index };
    publish_now(&snapshots, Job { offset, checkpoint, ts: listing.ts, engine, signing: other.clone() }).unwrap();
    let signers: BTreeSet<AccountId> = [signer(), AccountId(other.key.address())].into();
    let (_guard, mut alerts) = capture_alerts();
    assert!(usable(&snapshots, &dir, &signers, u64::MAX).await.is_none(), "the start passes it over");
    let alert = alerts.try_recv().expect("the start's refusal is paged");
    assert!(alert["message"].contains("its checkpoint is not the one the copy holds"), "{alert:?}");
    let signed = crate::daily_snapshots::open_signed(&snapshots.join(&listing.file), DOMAIN, &signers)
        .expect("signed by its checkpoint's recorded signer");
    let follower = open_under(&dir, Some(signed), signers);
    assert_eq!(follower.replays(), 1, "the replica replayed the whole copy");
    let alert = alerts.try_recv().expect("the refusal is paged");
    assert_eq!(alert["alert"].as_str(), "snapshot_start_refused");
    assert!(alert["message"].contains("the copy holds another entry at"), "{alert:?}");
}

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
    publish_now(&origin, Job { offset, checkpoint, ts: newer.ts, engine, signing: recorded() }).unwrap();
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
    std::fs::write(held.join(&kept.file), serde_json::to_vec(&damaged).unwrap()).unwrap();
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

/// A replica from a snapshot records no checkpoint it verified, so it answers no member's log check, whose range may
/// start before its snapshot; a whole replay records them.
#[tokio::test]
async fn a_replica_from_a_snapshot_answers_no_log_check() {
    let (dir, snapshots) = (scratch_dir("build-snapshot-checked"), scratch_dir("build-snapshot-checked-daily"));
    write_log(&dir).await;
    publish_middle(&dir, &snapshots, &recorded());
    let signed = usable(&snapshots, &dir, &[signer()].into(), u64::MAX).await.expect("a usable snapshot");
    let (from, whole) = (CheckedHeads::default(), CheckedHeads::default());
    let follower = open_with(&dir, Some(signed), [signer()].into(), Some(from.clone()));
    let last = follower.replica().last_checkpoint().expect("a checkpoint").seq;
    assert_eq!(follower.replays(), 0);
    assert!(from.at(last).is_none(), "the replica from the snapshot verified no range from the copy's start");
    open_with(&dir, None, [signer()].into(), Some(whole.clone()));
    assert!(whole.at(last).is_some(), "a whole replay verified every checkpoint");
}

// ── Helpers ──

/// Lists `listed` in the manifest of `snapshots`, in place of what it lists.
fn write_listings(snapshots: &Path, listed: &[Listed]) {
    let lines: String = listed.iter().map(|listing| serde_json::to_string(listing).unwrap() + "\n").collect();
    std::fs::write(snapshots.join(MANIFEST), lines).unwrap();
}

/// Writes a log of the dev genesis and screened credits and withdrawals, each authorized as mock custody does,
/// and ticks, across several checkpoints, ending at one (`write_funded_log`).
async fn write_log(dir: &Path) {
    crate::sequencer::fixture_tests::write_funded_log(dir, 4);
}

/// Publishes into `snapshots`, signed by `signing`, a daily snapshot of the copy in `dir` at its middle checkpoint
/// of a state with custody finality; its seq.
fn publish_middle(dir: &Path, snapshots: &Path, signing: &Signing) -> u64 {
    publish_at(dir, snapshots, signing, |held| held / 2)
}

/// `publish_middle`, at the checkpoint `pick` names among the copy's checkpoints of a state with custody finality,
/// given how many there are.
fn publish_at(dir: &Path, snapshots: &Path, signing: &Signing, pick: fn(usize) -> usize) -> u64 {
    let entries = log_entries(dir);
    let mut replica = LiveReplay::new(DOMAIN, [signer()].into());
    let (mut at, mut offsets) = (None, Vec::new());
    for entry in &entries {
        at = offset_of(at, entry).ok();
        if let LogEntry::Checkpoint(checkpoint) = entry
            && replica.engine().deposit_finality_enabled()
        {
            let engine = Encoding::Encoded(serde_json::to_string(replica.engine()).unwrap());
            offsets.push((at.unwrap(), checkpoint.clone(), replica.engine().now(), engine));
        }
        replica.apply(entry).unwrap();
    }
    let (offset, checkpoint, ts, engine) = offsets.swap_remove(pick(offsets.len()));
    let seq = offset.seq;
    publish_now(snapshots, Job { offset, checkpoint, ts, engine, signing: signing.clone() }).unwrap();
    seq
}

/// The snapshot in `snapshots` a replica of the copy in `dir` starts from, under `signers` and below `bound`, with no
/// origin to fetch from (`snapshot_start`).
async fn usable(snapshots: &Path, dir: &Path, signers: &BTreeSet<AccountId>, bound: u64) -> Option<Signed> {
    let client = crate::standby::archive::archive_client(false).unwrap();
    snapshot_start(&client, &[], (snapshots, dir), DOMAIN, signers, (bound, None)).await
}

/// The signing the test sequencer's own enclave does.
fn recorded() -> Signing {
    Signing { key: Arc::new(EnclaveKey::test_key()), domain: DOMAIN }
}

/// Rewrites the only listing in `snapshots`' manifest with `alter` applied.
fn rewrite_listing(snapshots: &Path, alter: impl FnOnce(&mut Listed)) {
    let mut listing = read_manifest(snapshots).unwrap().pop().unwrap();
    alter(&mut listing);
    std::fs::write(snapshots.join(MANIFEST), serde_json::to_string(&listing).unwrap() + "\n").unwrap();
}

/// The follower of the copy in `dir`, its replica built from `snapshot` or by a whole replay.
fn open(dir: &Path, snapshot: Option<Signed>) -> Follower {
    open_under(dir, snapshot, [signer()].into())
}

/// `open`, under the recorded `signers`.
fn open_under(dir: &Path, snapshot: Option<Signed>, signers: BTreeSet<AccountId>) -> Follower {
    open_with(dir, snapshot, signers, None)
}

/// `open_under`, recording the checkpoints the replica verifies in `checked`, as a member's does.
fn open_with(
    dir: &Path,
    snapshot: Option<Signed>,
    signers: BTreeSet<AccountId>,
    checked: Option<CheckedHeads>,
) -> Follower {
    let records = (None, None, checked, None);
    let workers = ReplayWorkers::available();
    let opened = Follower::open_pinned(
        dir,
        SEGMENT_COMMANDS,
        workers,
        DOMAIN,
        signer(),
        signers,
        records,
        (None, &[]),
        snapshot,
        |_| Ok(()),
    );
    opened.unwrap()
}

/// Serves the daily snapshots in `dir` as an archive does, over plain HTTP on loopback: its base URL.
async fn serve(dir: &Path) -> String {
    use axum::response::IntoResponse;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = dir.to_owned();
    let app = axum::Router::new().route(
        "/v1/archive/snapshots/{file}",
        axum::routing::get(move |axum::extract::Path(file): axum::extract::Path<String>| {
            let path = dir.join(&file);
            async move {
                match std::fs::read(path) {
                    Ok(bytes) => bytes.into_response(),
                    Err(_) => axum::http::StatusCode::NOT_FOUND.into_response(),
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// The replica's engine as JSON.
fn engine_json(follower: &Follower) -> serde_json::Value {
    serde_json::to_value(follower.replica().engine()).unwrap()
}

/// The test sequencer's enclave, which seals and signs its log.
fn signer() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}
