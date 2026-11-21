//! A member whose copy is empty starts its replica from the newest daily snapshot its archive lists whose
//! checkpoint the copy holds and the vault finalized, with no whole replay; a member without `[daily_snapshots]`,
//! one whose vault has finalized no checkpoint, and one whose copy holds the log already replay the whole copy; and
//! the snapshot is from before the turns of the member in office, as the schedule draws them.

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

/// The turns of the member in office began at the first of the turns in a row its candidate holds, read back past
/// the turns it holds to one another member holds; at the first turn when it holds every turn before; at the earliest
/// time when it holds more than are read back; and none are in office before the first turn. The member's own turns,
/// not yet taken, are passed over for the member's before them.
#[tokio::test]
async fn the_in_office_members_turns_began_at_the_first_it_holds_in_a_row() {
    let schedule = Schedule([1, 2, 2, 3, 3, 3].iter().zip(1..).map(|(holder, turn)| drawn(turn, *holder)).collect());
    let at = schedule.observe(View::Finalized, &[]).await.unwrap();
    let began = |now_ms| super::super::open::in_office_since(&schedule, &at, now_ms, [0xff; 32]);
    assert_eq!(began(boundary(6) + 1).await.unwrap(), Some(boundary(4)), "three turns in a row");
    assert_eq!(began(boundary(3)).await.unwrap(), Some(boundary(2)), "two turns in a row");
    assert_eq!(began(boundary(1)).await.unwrap(), Some(boundary(1)), "the first turn");
    assert_eq!(began(boundary(1) - 1).await.unwrap(), None, "before the first turn");
    let own = super::super::open::in_office_since(&schedule, &at, boundary(6), [3; 32]).await.unwrap();
    assert_eq!(own, Some(boundary(2)), "the member's own turns are passed over");
    let long = Schedule((1..=40).map(|turn| drawn(turn, 9)).collect());
    let at = long.observe(View::Finalized, &[]).await.unwrap();
    let began = super::super::open::in_office_since(&long, &at, boundary(40), [0xff; 32]).await.unwrap();
    assert_eq!(began, Some(Millis::MIN), "more turns in a row than are read back");
}

// ── Helpers ──

/// A schedule's drawn turns, oldest first.
struct Schedule(Vec<ScheduledTurn>);

impl CheckpointChain for Schedule {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        let zero = engine_core::Fixed::ZERO;
        let signer = AccountId([0; 20]);
        let (balance, deposited, distributions_paid) = (zero, zero, zero);
        Ok(VaultState { height: 1, signer, checkpoint: None, balance, deposits: 0, deposited, distributions_paid })
    }

    async fn post(&self, _checkpoint: &crate::log::Checkpoint) -> anyhow::Result<()> {
        anyhow::bail!("this vault takes no checkpoints")
    }

    async fn scheduled_turn(&self, _at: &Observation, now_ms: Millis) -> anyhow::Result<Option<ScheduledTurn>> {
        Ok(self.0.iter().rev().find(|turn| turn.boundary <= now_ms).cloned())
    }
}

/// Turn `turn`, drawn for the candidate `holder` names.
fn drawn(turn: u64, holder: u8) -> ScheduledTurn {
    let candidate = AccountId([holder; 20]);
    let boundary = boundary(turn);
    ScheduledTurn {
        turn,
        boundary,
        candidate,
        admin_key: candidate,
        release: [holder; 32],
        generation: 1,
        set: 1,
        source: None,
    }
}

/// The boundary of the stand-in schedule's turn `turn`.
fn boundary(turn: u64) -> Millis {
    turn as Millis * 1_000_000
}

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
