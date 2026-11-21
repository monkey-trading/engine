//! A follower's replica built from a daily snapshot, against one built by a whole replay of its copy.

use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::daily_snapshots::{Job, Listed, MANIFEST, Signing, publish_now, read_manifest, snapshot_start};
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
/// snapshot, is not taken: the copy holds another checkpoint at its offset, which the replica's build finds, paged as
/// Warning `snapshot_start_refused` before the whole replay.
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
    let signed =
        usable(&snapshots, &dir, &signers, u64::MAX).await.expect("signed by its checkpoint's recorded signer");
    let (_guard, mut alerts) = capture_alerts();
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
    let signed = snapshot_start(&client, &origins, (&member, &dir), DOMAIN, &[signer()].into(), u64::MAX).await;
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
    let none = snapshot_start(&client, &unreachable, (&empty, &dir), DOMAIN, &signers, u64::MAX);
    assert!(none.await.is_none());
    let alert = alerts.try_recv().expect("the silence is paged");
    assert!(alert["message"].contains("no origin's daily snapshot manifest could be read"), "{alert:?}");
}

// ── Helpers ──

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
    snapshot_start(&client, &[], (snapshots, dir), DOMAIN, signers, bound).await
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
    let records = (None, None, None, None);
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
