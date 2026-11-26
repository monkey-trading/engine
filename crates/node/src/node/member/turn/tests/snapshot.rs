//! A member starts its replica from the newest daily snapshot whose checkpoint its copy holds and the vault finalized,
//! of those its directory holds and those its archive lists, with no whole replay, whether its copy is empty or holds
//! the log already; a member without `[daily_snapshots]`, one whose vault has finalized no checkpoint, and one whose
//! snapshot is refused replay the whole copy; and the snapshot is from before the turns of the member in office, as
//! the schedule draws them.

use super::*;
use crate::config::DailySnapshotsSection;
use crate::daily_snapshots::{Job, Listed, Signing, publish_now, read_manifest};
use crate::log::{CheckpointDomain, LogEntry, offset_of};
use crate::sequencer::replay::{Carried, LiveReplay};
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
    fixture.cfg.daily_snapshots = Some(DailySnapshotsSection::at(daily.clone()));
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

/// A member whose copy holds the log already starts its replica from a snapshot too: one its archive lists, fetched,
/// when it follows an operator, and one its own directory holds when it follows none.
#[tokio::test]
async fn a_member_whose_copy_holds_the_log_starts_its_replica_from_a_snapshot() {
    let mut fixture = Fixture::start("member-snapshot-held", true).await;
    let origin = scratch_dir("member-snapshot-held-origin");
    let [_, newer] = publish(&fixture.log, &origin);
    let daily = scratch_dir("member-snapshot-held-daily");
    fixture.cfg.daily_snapshots = Some(DailySnapshotsSection::at(daily.clone()));
    *fixture.registry.finalized.lock().unwrap() = Some((newer.seq, newer.root.clone()));
    let followed = followed(archive_with(&fixture.log, &origin).await);
    let member = fixture.member();
    let whole = crate::sequencer::replay_path(&fixture.cfg.node.log_dir).expect("the copy replays");
    let (follower, ..) = member.open(Some(&followed)).await.expect("the member opens its copy");
    assert_eq!(follower.replays(), 0, "the replica from the archive's snapshot replayed nothing whole");
    assert_eq!(read_manifest(&daily).unwrap(), [newer], "the snapshot is fetched and kept");
    drop(follower);
    let (follower, ..) = member.open(None).await.expect("the member opens its copy");
    assert_eq!(follower.replays(), 0, "the replica from the held snapshot replayed nothing whole");
    assert_eq!(member.metrics.full_replays.get(), 0);
    assert_eq!(
        serde_json::to_value(follower.replica().engine()).unwrap(),
        serde_json::to_value(&whole.engine).unwrap()
    );
}

/// A member replays its whole copy, with the snapshots on offer, when it has no `[daily_snapshots]` and when the
/// vault has finalized no checkpoint.
#[tokio::test]
async fn a_member_replays_its_whole_copy_when_no_snapshot_start_applies() {
    for case in ["no [daily_snapshots]", "no finalized checkpoint"] {
        let mut fixture = Fixture::start("member-snapshot-whole", false).await;
        let origin = scratch_dir("member-snapshot-whole-origin");
        let [older, _] = publish(&fixture.log, &origin);
        if case != "no [daily_snapshots]" {
            fixture.cfg.daily_snapshots = Some(DailySnapshotsSection::at(scratch_dir("member-snapshot-whole-daily")));
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

/// A member hands the copy it opens to the archive's uploader, whose ranges it uploads while the member follows.
#[tokio::test]
async fn a_member_hands_the_copy_it_opens_to_the_archives_uploader() {
    let fixture = Fixture::start("member-archive", true).await;
    let origin = scratch_dir("member-archive-origin");
    publish(&fixture.log, &origin);
    let followed = followed(archive_with(&fixture.log, &origin).await);
    let (archive, handed) = tokio::sync::watch::channel(None);
    let delivered = crate::node::secrets::Delivered { archive: Some(archive), ..Default::default() };
    let member = fixture.member().with_feeds(&delivered);
    let (follower, ..) = member.open(Some(&followed)).await.expect("the member opens its copy");
    let manifest = handed.borrow().clone().expect("the member hands its copy's manifest to the uploader");
    let listed = manifest.borrow().unlisted_from().unwrap();
    assert_eq!(listed, follower.manifest().borrow().unlisted_from().unwrap());
    assert!(listed > 0, "the copy's manifest lists its segments");
}

/// A member whose only snapshot is refused replays its whole copy: one signed by a key the vault has not recorded and
/// one whose carried replay state does not stand for its checkpoint, each paged as Warning `snapshot_start_refused`,
/// and one of an earlier format it holds, passed over without a page.
#[tokio::test]
async fn a_member_whose_snapshot_is_refused_replays_its_whole_copy() {
    type Publish = fn(&std::path::Path, &std::path::Path, &std::path::Path) -> Listed;
    let cases: [(&str, Publish, Option<&str>); 3] = [
        (
            "an unrecorded signer",
            |log, origin, _| {
                let signing = Signing { key: Arc::new(EnclaveKey::generate()), domain: DOMAIN };
                publish_one(log, origin, &signing, |_| {})
            },
            Some("is not signed by"),
        ),
        (
            "a carried state that does not stand",
            |log, origin, _| publish_one(log, origin, &recorded(), |carried| _ = carried.links.pop()),
            Some("do not end at the checkpoint's command"),
        ),
        (
            "an earlier format",
            |log, _, daily| {
                let listing = publish_one(log, daily, &recorded(), |_| {});
                let path = daily.join(&listing.file);
                let written = std::fs::read_to_string(&path).unwrap();
                std::fs::write(&path, written.replacen("{\"schema\":3,", "{\"schema\":2,", 1)).unwrap();
                listing
            },
            None,
        ),
    ];
    for (case, publish, said) in cases {
        let mut fixture = Fixture::start("member-snapshot-refused", false).await;
        let (origin, daily) =
            (scratch_dir("member-snapshot-refused-origin"), scratch_dir("member-snapshot-refused-daily"));
        let listing = publish(&fixture.log, &origin, &daily);
        fixture.cfg.daily_snapshots = Some(DailySnapshotsSection::at(daily));
        *fixture.registry.finalized.lock().unwrap() = Some((listing.seq, listing.root.clone()));
        let followed = followed(archive_with(&fixture.log, &origin).await);
        let member = fixture.member();
        let (_guard, mut alerts) = capture_alerts();
        let (follower, ..) = member.open(Some(&followed)).await.expect("the member opens its copy");
        assert_eq!(follower.replays(), 1, "{case}");
        assert_eq!(member.metrics.full_replays.get(), 1, "{case}");
        let paged = std::iter::from_fn(|| alerts.try_recv().ok())
            .find(|alert| alert.get("alert").is_some_and(|name| name == "snapshot_start_refused"));
        match said {
            Some(said) => {
                let paged = paged.unwrap_or_else(|| panic!("{case}: nothing paged"));
                assert!(paged["message"].contains(said), "{case}: {paged:?}");
            }
            None => assert!(paged.is_none(), "{case}: {paged:?}"),
        }
    }
}

/// A replica's snapshot precedes the first seq it keeps, and stands close enough after the posted head that the links
/// it carries reach back to it; with neither, any snapshot will do.
#[test]
fn a_snapshot_precedes_the_kept_seqs_and_reaches_back_to_the_posted_head() {
    let keep_from = super::super::open::keep_from;
    let reach = crate::sequencer::replay::RETAINED_LINKS as u64;
    let posted = Some((1_000, B256::repeat_byte(1)));
    assert_eq!(keep_from(&[], None), None);
    assert_eq!(keep_from(&[], posted), Some(1_000 + reach), "within the carried links of the posted head");
    assert_eq!(keep_from(&[9, 5], posted), Some(5), "before the first kept seq");
    assert_eq!(keep_from(&[5_000], posted), Some(1_000 + reach));
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
/// of a state with custody finality, whose segments the archive holds, each carrying what a whole replay of the log
/// carries there; their listings.
fn publish(log: &std::path::Path, dir: &std::path::Path) -> [Listed; 2] {
    let mut listed = checkpoints(log).into_iter().skip(1).take(2).map(|(offset, checkpoint, ts, engine)| {
        let carried = crate::sequencer::replay::carried_at(
            log,
            offset,
            (DOMAIN, [AccountId(EnclaveKey::test_key().address())].into()),
        )
        .unwrap();
        publish_now(dir, job((offset, checkpoint, ts), engine, carried, &recorded())).unwrap()
    });
    [listed.next().expect("a second checkpoint"), listed.next().expect("a third checkpoint")]
}

/// Publishes into `dir`, signed by `signing`, a daily snapshot of the operator's log in `log` at its second
/// checkpoint of a state with custody finality, carrying what a whole replay of the log carries there with `alter`
/// applied; its listing.
fn publish_one(
    log: &std::path::Path,
    dir: &std::path::Path,
    signing: &Signing,
    alter: impl FnOnce(&mut Carried),
) -> Listed {
    let (offset, checkpoint, ts, engine) = checkpoints(log).swap_remove(1);
    let mut carried: Carried = serde_json::from_str(
        &crate::sequencer::replay::carried_at(
            log,
            offset,
            (DOMAIN, [AccountId(EnclaveKey::test_key().address())].into()),
        )
        .unwrap(),
    )
    .unwrap();
    alter(&mut carried);
    let carried = serde_json::to_string(&carried).unwrap();
    publish_now(dir, job((offset, checkpoint, ts), engine, carried, signing)).unwrap()
}

/// Each checkpoint of a state with custody finality in the log in `log`: its offset, the checkpoint, its stamp and
/// the engine's encoding there.
fn checkpoints(log: &std::path::Path) -> Vec<(crate::log::Offset, Checkpoint, Millis, String)> {
    let mut replica = LiveReplay::new(DOMAIN, [AccountId(EnclaveKey::test_key().address())].into());
    let (mut at, mut found) = (None, Vec::new());
    for entry in log_entries(log) {
        at = offset_of(at, &entry).ok();
        if let LogEntry::Checkpoint(checkpoint) = &entry
            && replica.engine().deposit_finality_enabled()
        {
            let engine = serde_json::to_string(replica.engine()).unwrap();
            found.push((at.unwrap(), checkpoint.clone(), replica.engine().now(), engine));
        }
        replica.apply(&entry).unwrap();
    }
    found
}

/// A daily snapshot job of `engine` and `carried` at the checkpoint `at` names, signed by `signing`.
fn job(
    (offset, checkpoint, ts): (crate::log::Offset, Checkpoint, Millis),
    engine: String,
    carried: String,
    signing: &Signing,
) -> Job {
    let (engine, carried, history) =
        (Encoding::Encoded(engine), Encoding::Encoded(carried), crate::daily_snapshots::Pending::none());
    Job { offset, checkpoint, ts, engine, carried, history, signing: signing.clone() }
}

/// The signing the test sequencer's own enclave does.
fn recorded() -> Signing {
    Signing { key: Arc::new(EnclaveKey::test_key()), domain: DOMAIN }
}

/// The operator the member follows, at `origin`, whose archive the member backfills from.
fn followed(origin: String) -> Followed {
    let enclave = AccountId(EnclaveKey::test_key().address());
    Followed { origin, enclave, mirrors: Vec::new(), danger_accept_invalid_certs: true, bucket: None }
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
