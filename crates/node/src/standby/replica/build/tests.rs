//! A follower's replica built from a daily snapshot, against one built by a whole replay of its copy.

use std::path::Path;
use std::sync::Arc;

use engine_core::Engine;

use super::*;
use crate::daily_snapshots::{Job, Listed, MANIFEST, Signing, publish_now, read_manifest, snapshot_start};
use crate::log_check::answer::CheckedHeads;
use crate::sequencer::replay::Carried;
use crate::snapshot::sealing::Encoding;
use crate::tee::EnclaveKey;
use crate::test_support::capture_alerts;
use crate::test_support::{SEGMENT_COMMANDS, log_entries, scratch_dir};

mod fetch;
mod local;
mod refused;
mod vault_root;

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

/// A snapshot its checkpoint's recorded signer signed whose state or carried replay state does not stand for its
/// checkpoint is refused as the replica goes on from it, paged as Warning `snapshot_start_refused`, and the replica
/// replays the whole copy instead: a balance its checkpoint's root does not hold, an approval out of the payout
/// order, and carried links that end before its checkpoint's command.
#[tokio::test]
async fn a_snapshot_that_does_not_stand_for_its_checkpoint_falls_back_to_a_whole_replay() {
    let dir = scratch_dir("build-snapshot-stand");
    write_log(&dir).await;
    let whole = engine_json(&open(&dir, None));
    type Alter = fn(&mut serde_json::Value, &mut Carried);
    let cases: [(&str, Alter, &str); 3] = [
        (
            "a balance the root does not hold",
            |engine, _| {
                let accounts = engine["accounts"].as_object_mut().unwrap();
                accounts.values_mut().next().unwrap()["balance"] = serde_json::json!("1");
            },
            "state root mismatch",
        ),
        (
            "an approval out of the payout order",
            |_, carried| _ = carried.payout_order.pop().expect("the snapshot holds an approval"),
            "payout order is not the engine's approvals",
        ),
        (
            "links that end before its checkpoint's command",
            |_, carried| _ = carried.links.pop(),
            "do not end at the checkpoint's command",
        ),
    ];
    for (case, alter, said) in cases {
        let snapshots = scratch_dir("build-snapshot-stand-daily");
        // The last checkpoint, whose state holds approvals.
        publish_altered(&dir, &snapshots, &recorded(), |held| held - 1, |_, engine, carried| alter(engine, carried));
        let (_guard, mut alerts) = capture_alerts();
        let signed = usable(&snapshots, &dir, &[signer()].into(), u64::MAX).await.expect(case);
        let follower = open(&dir, Some(signed));
        assert_eq!(follower.replays(), 1, "{case}: the replica replayed the whole copy");
        assert_eq!(engine_json(&follower), whole, "{case}");
        let alert = alerts.try_recv().unwrap_or_else(|_| panic!("{case}: nothing paged"));
        assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("snapshot_start_refused", "WARN"), "{case}");
        assert!(alert["message"].contains(said), "{case}: {alert:?}");
    }
}

/// A snapshot no replica starts from is passed over, paged as Warning `snapshot_start_refused`: one whose state
/// or carried replay state is not what its signature covers, one signed by a key the vault has not recorded, one
/// signed for another deployment, and one of a checkpoint the copy does not hold. One of a checkpoint past the
/// bound, the vault's finalized one, is not tried.
#[tokio::test]
async fn a_snapshot_a_replica_cannot_start_from_is_passed_over() {
    let dir = scratch_dir("build-snapshot-refused");
    write_log(&dir).await;
    let recorded = recorded();
    let elsewhere = CheckpointDomain { chain_id: 1, vault: AccountId([0x11; 20]) };
    // Each alters the published file in place.
    type Alter = fn(&Path);
    let cases: [(&str, Option<Alter>, Signing, &str); 5] = [
        (
            "an altered state",
            Some(|path| {
                rewrite_snapshot(path, |snapshot| snapshot["engine"]["accounts"].as_object_mut().unwrap().clear())
            }),
            recorded.clone(),
            "is not the state its signature covers",
        ),
        (
            "an altered carried state",
            Some(|path| {
                // The carried state's bytes alone, the engine's left as written.
                let written = std::fs::read_to_string(path).unwrap();
                let altered = written.replacen("\"carried\":{\"commands\":", "\"carried\":{\"commands\":1", 1);
                assert_ne!(altered, written, "the snapshot carries a command count");
                std::fs::write(path, altered).unwrap();
            }),
            recorded.clone(),
            "is not signed by",
        ),
        (
            "an unrecorded signer",
            None,
            Signing { key: Arc::new(EnclaveKey::generate()), domain: DOMAIN },
            "is not signed by",
        ),
        ("another deployment", None, Signing { key: recorded.key.clone(), domain: elsewhere }, "is not signed by"),
        (
            "a checkpoint the copy does not hold",
            Some(|path| {
                rewrite_snapshot(path, |snapshot| snapshot["checkpoint"]["root"] = serde_json::json!(unheld_root()))
            }),
            recorded.clone(),
            "the copy holds no checkpoint",
        ),
    ];
    for (case, alter, signing, said) in cases {
        let snapshots = scratch_dir("build-snapshot-refused-daily");
        let seq = publish_middle(&dir, &snapshots, &signing);
        let listing = read_manifest(&snapshots).unwrap().pop().unwrap();
        if let Some(alter) = alter {
            alter(&snapshots.join(&listing.file));
        }
        if case == "a checkpoint the copy does not hold" {
            rewrite_listing(&snapshots, |listing| listing.root = unheld_root());
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
/// snapshot and which its carried state names, is not taken: the copy holds another checkpoint at its offset,
/// which the start finds, and the replica's build finds too should one reach it, each paged as Warning
/// `snapshot_start_refused`, the build before the whole replay.
#[tokio::test]
async fn a_snapshot_of_another_signers_checkpoint_falls_back_to_a_whole_replay() {
    let (dir, snapshots) = (scratch_dir("build-snapshot-signer"), scratch_dir("build-snapshot-signer-daily"));
    write_log(&dir).await;
    let other = Signing { key: Arc::new(EnclaveKey::generate()), domain: DOMAIN };
    let resigned = |checkpoint: &mut Checkpoint, _: &mut serde_json::Value, carried: &mut Carried| {
        let root: B256 = checkpoint.root.parse().unwrap();
        checkpoint.signature = other.key.sign_checkpoint(DOMAIN.chain_id, &DOMAIN.vault.0, checkpoint.seq, &root.0);
        checkpoint.signer = other.key.address_hex();
        carried.checkpoint_signers.insert(AccountId(other.key.address()));
    };
    publish_altered(&dir, &snapshots, &other, |held| held / 2, resigned);
    let listing = read_manifest(&snapshots).unwrap().pop().unwrap();
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

/// A copy with a tail no accepted signer sealed, after the checkpoint a signed snapshot stands at, is cut back and its
/// replica built again from the snapshot, with no whole replay: the tail is kept aside, paged once as Critical
/// `copy_tail_unaccepted`, and the replica holds what a whole replay of the cut copy holds.
#[tokio::test]
async fn a_replica_from_a_snapshot_is_rebuilt_after_an_unaccepted_tail_is_cut() {
    let (dir, snapshots) = (scratch_dir("build-snapshot-cut"), scratch_dir("build-snapshot-cut-daily"));
    write_log(&dir).await;
    let seq = publish_middle(&dir, &snapshots, &recorded());
    // The copy's last checkpoints dropped, as an archive whose segments end between checkpoints lists, and the tail
    // after the one left sealed by a key the vault has not recorded.
    while crate::standby::copies_tests::uncovered(&dir).len() < 2 {
        crate::test_support::drop_last_checkpoint(&dir);
    }
    let tail = crate::standby::copies_tests::uncovered(&dir);
    let foreign = EnclaveKey::generate();
    crate::standby::copies_tests::seal_with(&dir, &foreign, &tail);
    let last = log_entries(&dir).into_iter().rev().find_map(|entry| match entry {
        LogEntry::Checkpoint(checkpoint) => Some(checkpoint.seq),
        _ => None,
    });
    assert!(last > Some(seq), "the snapshot's checkpoint precedes the tail's");
    let signed = usable(&snapshots, &dir, &[signer()].into(), u64::MAX).await.expect("a usable snapshot");
    let (_guard, mut alerts) = capture_alerts();
    let follower = open(&dir, Some(signed));
    assert_eq!(follower.replays(), 0, "the rebuild after the cut replays nothing whole");
    assert_eq!(follower.replica().last_seq(), Some(tail[0] - 1), "the tail was cut");
    let alert = alerts.try_recv().expect("the tail is paged");
    assert_eq!(
        (alert["alert"].as_str(), alert["level"].as_str(), alert["seq"].as_str()),
        ("copy_tail_unaccepted", "ERROR", tail[0].to_string().as_str())
    );
    assert!(alerts.try_recv().is_err(), "the page is the only one: the snapshot is not refused");
    let mut aside = dir.as_os_str().to_owned();
    aside.push(format!(".unaccepted-{}", tail[0]));
    assert!(std::path::Path::new(&aside).is_dir(), "the tail is kept aside");
    let whole = open(&dir, None);
    assert_eq!(whole.replays(), 1);
    assert_eq!(follower.replica().head_link(), whole.replica().head_link());
    assert_eq!(follower.replica().last_checkpoint(), whole.replica().last_checkpoint());
    assert_eq!(engine_json(&follower), engine_json(&whole));
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

/// Rewrites the daily snapshot at `path` with `alter` applied to its fields (`write_snapshot`).
fn rewrite_snapshot(path: &Path, alter: impl FnOnce(&mut serde_json::Value)) {
    let mut written: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    alter(&mut written);
    write_snapshot(path, &written);
}

/// A checkpoint root the test logs hold at no seq.
fn unheld_root() -> String {
    format!("0x{}", hex::encode([3; 32]))
}

/// Writes `snapshot`, a daily snapshot's fields, to `path` with its format first, as the writer puts it, so a start
/// reads the format it names from the file's first bytes.
fn write_snapshot(path: &Path, snapshot: &serde_json::Value) {
    let mut fields = snapshot.as_object().unwrap().clone();
    let schema = fields.remove("schema").unwrap();
    let rest = serde_json::to_string(&fields).unwrap();
    std::fs::write(path, format!("{{\"schema\":{schema},{}", &rest[1..])).unwrap();
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
    publish_altered(dir, snapshots, signing, pick, |_, _, _| {})
}

/// `publish_at`, carrying what a whole replay of the copy carries at its checkpoint, with `alter` applied to its
/// checkpoint, engine and carried state before it is signed.
fn publish_altered(
    dir: &Path,
    snapshots: &Path,
    signing: &Signing,
    pick: fn(usize) -> usize,
    alter: impl FnOnce(&mut Checkpoint, &mut serde_json::Value, &mut Carried),
) -> u64 {
    let entries = log_entries(dir);
    let mut replica = LiveReplay::new(DOMAIN, [signer()].into());
    let (mut at, mut offsets) = (None, Vec::new());
    for entry in &entries {
        at = offset_of(at, entry).ok();
        if let LogEntry::Checkpoint(checkpoint) = entry
            && replica.engine().deposit_finality_enabled()
        {
            let engine = serde_json::to_value(replica.engine()).unwrap();
            offsets.push((at.unwrap(), checkpoint.clone(), replica.engine().now(), engine));
        }
        replica.apply(entry).unwrap();
    }
    let (offset, mut checkpoint, ts, mut engine) = offsets.swap_remove(pick(offsets.len()));
    let mut carried: Carried =
        serde_json::from_str(&crate::sequencer::replay::carried_at(dir, offset, (DOMAIN, [signer()].into())).unwrap())
            .unwrap();
    alter(&mut checkpoint, &mut engine, &mut carried);
    let engine: Engine = serde_json::from_value(engine).unwrap();
    let (engine, carried) = (serde_json::to_string(&engine).unwrap(), serde_json::to_string(&carried).unwrap());
    let (engine, carried, history) =
        (Encoding::Encoded(engine), Encoding::Encoded(carried), crate::daily_snapshots::Pending::none());
    publish_now(snapshots, Job { offset, checkpoint, ts, engine, carried, history, signing: signing.clone() }).unwrap();
    offset.seq
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
    try_open(dir, snapshot, signers, checked).unwrap()
}

/// `open_with`, or why the copy does not open.
/// Error cases:
/// - as `Follower::open_pinned`
fn try_open(
    dir: &Path,
    snapshot: Option<Signed>,
    signers: BTreeSet<AccountId>,
    checked: Option<CheckedHeads>,
) -> anyhow::Result<Follower> {
    let records = (None, None, checked, None);
    let workers = ReplayWorkers::available();
    Follower::open_pinned(
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
    )
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
