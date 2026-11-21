//! A member whose copy is empty starts its replica from the newest daily snapshot its archive lists whose
//! checkpoint the copy holds and the vault finalized, with no whole replay; a member without `[daily_snapshots]`,
//! one whose vault has finalized no checkpoint, and one whose copy holds the log already replay the whole copy.

use super::*;
use crate::config::DailySnapshotsSection;
use crate::daily_snapshots::{Job, Listed, Signing, publish_now, read_manifest};
use crate::log::{CheckpointDomain, LogEntry, offset_of};
use crate::sequencer::replay::LiveReplay;
use crate::snapshot::sealing::Encoding;
use crate::standby::Followed;

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// The member bootstraps its empty copy from the archive, fetches the newest signed snapshot the archive lists whose
/// checkpoint the vault finalized, not a newer one, keeps it in its own directory, and builds its replica from it:
/// no whole replay, and the state a whole replay of the copy gives.
#[tokio::test]
async fn a_member_with_an_empty_copy_starts_its_replica_from_the_newest_snapshot_the_vault_finalized() {
    let mut fixture = Fixture::start("member-snapshot", false).await;
    let origin = scratch_dir("member-snapshot-origin");
    let [older, newer] = publish(&fixture.log, &origin);
    let daily = scratch_dir("member-snapshot-daily");
    fixture.cfg.daily_snapshots = Some(DailySnapshotsSection { dir: daily.clone() });
    *fixture.registry.finalized.lock().unwrap() = Some((older.seq, older.root.clone()));
    let followed = followed(archive_with(&fixture.log, &origin).await);
    let member = fixture.member();
    let (follower, ..) = member.open(Some(&followed)).await.expect("the member opens its copy");
    assert_eq!(follower.replays(), 0, "the replica replayed nothing whole");
    assert_eq!(member.metrics.full_replays.get(), 0);
    let kept: Vec<u64> = read_manifest(&daily).unwrap().iter().map(|listing| listing.seq).collect();
    assert_eq!(kept, [older.seq], "the snapshot the vault finalized, not seq {}", newer.seq);
    let whole = crate::sequencer::replay_path(&fixture.cfg.node.log_dir).expect("the copy replays");
    assert_eq!(
        serde_json::to_value(follower.replica().engine()).unwrap(),
        serde_json::to_value(&whole.engine).unwrap()
    );
}

/// A member replays its whole copy, with the snapshots on offer, when it has no `[daily_snapshots]`, when the vault
/// has finalized no checkpoint, and when its copy holds the log already.
#[tokio::test]
async fn a_member_replays_its_whole_copy_when_no_snapshot_start_applies() {
    for case in ["no [daily_snapshots]", "no finalized checkpoint", "a copy that holds the log"] {
        let mut fixture = Fixture::start("member-snapshot-whole", case == "a copy that holds the log").await;
        let origin = scratch_dir("member-snapshot-whole-origin");
        let [older, _] = publish(&fixture.log, &origin);
        if case != "no [daily_snapshots]" {
            fixture.cfg.daily_snapshots =
                Some(DailySnapshotsSection { dir: scratch_dir("member-snapshot-whole-daily") });
        }
        if case != "no finalized checkpoint" {
            *fixture.registry.finalized.lock().unwrap() = Some((older.seq, older.root.clone()));
        }
        let followed = followed(archive_with(&fixture.log, &origin).await);
        let member = fixture.member();
        let (follower, ..) = member.open(Some(&followed)).await.expect("the member opens its copy");
        assert_eq!(follower.replays(), 1, "{case}");
        assert_eq!(member.metrics.full_replays.get(), 1, "{case}");
    }
}

// ── Helpers ──

/// Publishes into `dir` signed daily snapshots of the operator's log in `log` at its second and third checkpoints
/// of a state with custody finality, whose segments the archive holds; their listings.
fn publish(log: &std::path::Path, dir: &std::path::Path) -> [Listed; 2] {
    let signing = Signing { key: Arc::new(EnclaveKey::test_key()), domain: DOMAIN };
    let mut replica = LiveReplay::new(DOMAIN, [AccountId(EnclaveKey::test_key().address())].into());
    let (mut at, mut published) = (None, Vec::new());
    for entry in log_entries(log) {
        at = offset_of(at, &entry).ok();
        if let LogEntry::Checkpoint(checkpoint) = &entry
            && replica.engine().deposit_finality_enabled()
        {
            published.push((
                at.unwrap(),
                checkpoint.clone(),
                replica.engine().now(),
                serde_json::to_string(replica.engine()).unwrap(),
            ));
        }
        replica.apply(&entry).unwrap();
    }
    let mut listed = published.into_iter().skip(1).take(2).map(|(offset, checkpoint, ts, engine)| {
        let job = Job { offset, checkpoint, ts, engine: Encoding::Encoded(engine), signing: signing.clone() };
        publish_now(dir, job).unwrap()
    });
    [listed.next().expect("a second checkpoint"), listed.next().expect("a third checkpoint")]
}

/// The operator the member follows, at `origin`, whose archive the member backfills from.
fn followed(origin: String) -> Followed {
    let enclave = AccountId(EnclaveKey::test_key().address());
    Followed { origin, enclave, mirrors: Vec::new(), danger_accept_invalid_certs: true }
}

/// Serves the completed segments of the log in `log`, their manifest and the daily snapshots in `snapshots` as an
/// archive, over plain HTTP on loopback: its base URL.
async fn archive_with(log: &std::path::Path, snapshots: &std::path::Path) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let files = |dir: &std::path::Path| {
        let dir = dir.to_owned();
        get(move |axum::extract::Path(file): axum::extract::Path<String>| {
            let path = dir.join(&file);
            async move {
                match std::fs::read(path) {
                    Ok(bytes) => bytes.into_response(),
                    Err(_) => StatusCode::NOT_FOUND.into_response(),
                }
            }
        })
    };
    let app =
        Router::new().route("/v1/archive/{file}", files(log)).route("/v1/archive/snapshots/{file}", files(snapshots));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}
