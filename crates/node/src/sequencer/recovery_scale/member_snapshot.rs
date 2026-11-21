//! A new member's start from a daily snapshot, which `scripts/dev-member-snapshot-start.sh` runs, on deterministic
//! time with isolated temporary state: a node writing launch flow on a small book across a UTC midnight publishes a
//! signed daily snapshot at its first checkpoint and at its first past midnight; a member with an empty copy
//! backfills the node's archive over HTTP, fetches from it the newest snapshot its copy holds, and builds its replica
//! from it with no whole replay, to the state, command prefix and last checkpoint a whole replay of the copy gives;
//! it then takes the node's turn through the tail handover and stamps its first command; and a snapshot whose state
//! its signer did not vouch for is passed over, paged as Warning `snapshot_start_refused`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use engine_core::types::AccountId;
use tokio::net::TcpListener;

use crate::clock::Clock;
use crate::common::launch_book::{NamedCounts, build};
use crate::config::HistorySection;
use crate::daily_snapshots::{self, snapshot_start};
use crate::history::derived::RowsStore;
use crate::log::ReplayWorkers;
use crate::sequencer::recovery_scale::flow::{Flow, Node};
use crate::sequencer::recovery_scale::handover::{build_replica, hand_over, member_config, start_from_replica};
use crate::sequencer::recovery_scale::orders::Traders;
use crate::sequencer::recovery_scale::restart::DOMAIN;
use crate::sequencer::recovery_scale::{Base, mix};
use crate::standby::archive::{archive_client, bootstrap_archive};
use crate::tee::EnclaveKey;
use crate::test_support::capture_alerts;
use crate::test_support::scratch_dir;

/// A UTC day, and how long before midnight the flow starts and how long it runs.
const DAY_MS: i64 = 86_400_000;
const BEFORE_MIDNIGHT: Duration = Duration::from_secs(600);
const FLOW: Duration = Duration::from_secs(1_200);

#[test]
fn dev_scenario_member_snapshot_start() {
    let counts = NamedCounts { adders: 1, openers: 8, closeouts: 0 };
    let mut book = build(&mix(200, 0, 1_200), counts);
    let engine = std::mem::take(&mut book.h.engine);
    let base = Base::new(&engine).expect("the book encodes");
    let traders = Traders::new(std::mem::take(&mut book.named.openers), &engine);
    drop((book, engine));
    let run = scratch_dir("member-snapshot-start");
    let (log, snapshots, copy) = (run.join("log"), run.join("snapshots"), run.join("copy"));
    let key = Arc::new(EnclaveKey::generate());
    let signer = AccountId(key.address());

    // The node publishes a snapshot at its first checkpoint, and at its first after midnight.
    let mut node = Node::publishing(base.engine(), &log, key.clone(), Some(&snapshots));
    let to_midnight = DAY_MS - node.clock.now_ms().rem_euclid(DAY_MS);
    node.clock.advance(Duration::from_millis(u64::try_from(to_midnight).unwrap()) - BEFORE_MIDNIGHT);
    let mut flow = Flow::new(traders, BTreeMap::from([(1, 11.0)]));
    flow.run(&mut node, FLOW, 1.0, |_, _, _| {});
    node.checkpoint_now();
    let midnight = published(&snapshots, 2).pop().expect("the snapshot past midnight");
    assert!(midnight.ts.rem_euclid(DAY_MS) < 60_000, "the second is at the first checkpoint past midnight");

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    runtime.block_on(async {
        // A member with an empty copy backfills the node's archive and fetches the newest snapshot its copy holds.
        let origins = [serve(&log, &snapshots).await];
        let client = archive_client(false).expect("an archive client");
        bootstrap_archive(&client, &origins, &copy).await.expect("the archive backfills the copy");
        let held = crate::log::last_checkpoint(&copy).expect("the copy reads").expect("a checkpoint").seq;
        let member_snapshots = run.join("member-snapshots");
        let signers = [signer].into();
        let signed = snapshot_start(&client, &origins, (&member_snapshots, &copy), DOMAIN, &signers, held)
            .await
            .expect("the snapshot is usable");
        assert_eq!(signed.snapshot.offset.seq, midnight.seq, "the member took the snapshot past midnight");
        assert_eq!(
            daily_snapshots::read_manifest(&member_snapshots).unwrap(),
            std::slice::from_ref(&midnight),
            "and keeps it"
        );

        // Its replica from the snapshot holds what a whole replay of the copy gives, with no whole replay.
        let workers = ReplayWorkers::available();
        let history = HistorySection { dir: run.join("member-history"), ..crate::test_support::dev_config().history };
        let rows = || Arc::new(RowsStore::new(&run.join("history"), history.retention_ms().expect("a retention")));
        let (whole, _, whole_took) = build_replica(&base, (&copy, None), (signer, workers), (rows(), &history));
        let expected = (whole.replica().head_link(), whole.replica().last_checkpoint().cloned(), engine_of(&whole));
        assert_eq!(whole.replays(), 1);
        drop(whole);
        let started = Instant::now();
        let (mut follower, stats, _) =
            build_replica(&base, (&copy, Some(signed)), (signer, workers), (rows(), &history));
        let took = started.elapsed();
        assert_eq!(follower.replays(), 0, "the member replayed nothing whole");
        let built =
            (follower.replica().head_link(), follower.replica().last_checkpoint().cloned(), engine_of(&follower));
        assert_eq!(built, expected, "the replica from the snapshot is the whole replay's");
        println!(
            "MEMBER replica commands={} whole_s={:.2} from_snapshot_s={:.2} snapshot_seq={}",
            follower.replica().last_seq().unwrap_or(0) + 1,
            whole_took.as_secs_f64(),
            took.as_secs_f64(),
            midnight.seq
        );

        // It takes the node's turn through the tail handover and stamps its first command.
        let cfg = member_config(&run);
        let peak = flow.peak_commands_per_sec() * 600.0;
        let switch = hand_over((&mut node, &mut flow, &key), (&mut follower, &cfg, 1), (64 << 10, peak), &log).await;
        assert!(switch.confirmed, "the node confirmed the handover");
        let (member, start) = start_from_replica(follower, stats, &copy, key.clone());
        assert!(member.seq.last_seq() > Some(held), "the member sequences past the node's head");
        println!("MEMBER turn tail_bytes={} start_s={:.3}", switch.bytes, start.as_secs_f64());

        // A snapshot whose state its signer did not vouch for is passed over, and paged.
        let path = member_snapshots.join(&midnight.file);
        let mut written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let accounts = written["engine"]["accounts"].as_object_mut().expect("the accounts");
        accounts.values_mut().next().expect("an account")["balance"] = serde_json::json!("1");
        std::fs::write(&path, serde_json::to_vec(&written).unwrap()).unwrap();
        let (_guard, mut alerts) = capture_alerts();
        let refused = snapshot_start(&client, &[], (&member_snapshots, &copy), DOMAIN, &signers, held).await;
        assert!(refused.is_none(), "an altered snapshot is not used");
        let alert = alerts.try_recv().expect("the refusal is paged");
        assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("snapshot_start_refused", "WARN"));
    });
}

// ── Helpers ──

/// The snapshots listed in `dir` once `count` are, waiting for the node's writer, which publishes on a thread of
/// its own.
fn published(dir: &Path, count: usize) -> Vec<daily_snapshots::Listed> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let listed = daily_snapshots::read_manifest(dir).expect("the manifest");
        if listed.len() >= count {
            return listed;
        }
        assert!(Instant::now() < deadline, "the writer published {} of {count} snapshots", listed.len());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Serves the log in `log` and the daily snapshots in `snapshots` as a node's archive does, over plain HTTP on
/// loopback: its base URL.
async fn serve(log: &Path, snapshots: &Path) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback listener");
    let url = format!("http://{}", listener.local_addr().expect("its address"));
    let file = |dir: &Path| {
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
        Router::new().route("/v1/archive/{file}", file(log)).route("/v1/archive/snapshots/{file}", file(snapshots));
    tokio::spawn(async move { axum::serve(listener, app).await.expect("the archive serves") });
    url
}

/// The replica's engine as JSON.
fn engine_of(follower: &crate::standby::Follower) -> serde_json::Value {
    serde_json::to_value(follower.replica().engine()).expect("the engine encodes")
}
