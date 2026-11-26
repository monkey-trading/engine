//! A member's fetch of a daily snapshot from the archives of its origins.

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::net::TcpListener;

use super::*;
use crate::daily_snapshots::{DAY_MS, Job, Listed, Signing, publish_now};
use crate::log::Checkpoint;
use crate::snapshot::sealing::Encoding;
use crate::test_support::scratch_dir;
use engine_core::types::AccountId;

/// A member hears the snapshots each origin lists at or before the bound, under their canonical names, with the
/// origin that lists them: an origin that publishes none answers, one that cannot be reached does not.
#[tokio::test]
async fn a_member_hears_the_snapshots_its_origins_list_within_the_bound() {
    let (older, newer) = (scratch_dir("fetch-older"), scratch_dir("fetch-newer"));
    publish(&older, 5, 0);
    publish(&newer, 5, 0);
    let latest = publish(&newer, 9, DAY_MS);
    let (none, older_url, newer_url) =
        (serve(&scratch_dir("fetch-none")).await, serve(&older).await, serve(&newer).await);
    let client = crate::standby::archive::archive_client(false).unwrap();
    let origins = vec![none.clone(), older_url.clone(), newer_url.clone()];

    let (offers, answered) = offered(&client, &origins, 8).await;
    assert!(answered);
    let heard: Vec<(&str, u64)> = offers.iter().map(|(origin, listing)| (origin.as_str(), listing.seq)).collect();
    assert_eq!(heard, [(older_url.as_str(), 5), (newer_url.as_str(), 5)], "seq 9 is past the bound");
    let (offers, _) = offered(&client, &origins, u64::MAX).await;
    assert_eq!(offers.last().map(|(_, listing)| listing), Some(&latest));

    // An origin that publishes no snapshots answers; one that cannot be reached does not.
    assert!(offered(&client, &[none], u64::MAX).await.1, "a 404 is an answer");
    let unreachable = "http://127.0.0.1:1".to_owned();
    let (offers, answered) = offered(&client, &[unreachable], u64::MAX).await;
    assert!(offers.is_empty() && !answered);

    // A listing under a name that is not its offset's is not heard.
    let mut misnamed = latest.clone();
    misnamed.index = 2;
    let mut escaping = latest.clone();
    escaping.file = "../escape.json".into();
    let lines: String =
        [misnamed, escaping].iter().map(|listing| serde_json::to_string(listing).unwrap() + "\n").collect();
    std::fs::write(newer.join(MANIFEST), lines).unwrap();
    assert!(offered(&client, &[newer_url], u64::MAX).await.0.is_empty());
}

/// A member fetches a listed snapshot byte for byte, unlisted, into its directory; one whose bytes differ from its
/// listing is refused, as is one whose first bytes name an earlier format, before the rest is read, and nothing of
/// either stays.
#[tokio::test]
async fn a_member_fetches_a_snapshot_whole_or_not_at_all() {
    let origin = scratch_dir("fetch-origin");
    let listing = publish(&origin, 5, 0);
    let url = serve(&origin).await;
    let client = crate::standby::archive::archive_client(false).unwrap();
    let member = scratch_dir("fetch-member");
    let path = download(&client, &url, &member, &listing, START).await.unwrap();
    assert_eq!(path, member.join(&listing.file));
    assert_eq!(std::fs::read(&path).unwrap(), std::fs::read(origin.join(&listing.file)).unwrap());
    assert!(!member.join(MANIFEST).exists(), "the caller lists it once it passes");

    let written = std::fs::read(origin.join(&listing.file)).unwrap();
    let mut damaged = written.clone();
    damaged[40] ^= 1;
    let mut earlier = written.clone();
    let prefix = format!("{{\"schema\":{},", crate::daily_snapshots::SCHEMA).into_bytes();
    assert!(earlier.starts_with(&prefix), "the writer puts the format first");
    earlier[prefix.len() - 2] = b'2';
    for (name, bytes) in [("damaged", damaged), ("earlier", earlier)] {
        std::fs::write(origin.join(&listing.file), bytes).unwrap();
        let member = scratch_dir(&format!("fetch-{name}-member"));
        let error = download(&client, &url, &member, &listing, START).await.unwrap_err();
        match name {
            "damaged" => assert!(format!("{error:#}").contains("differs from its listing"), "{error:#}"),
            _ => assert!(error.is::<EarlierFormat>(), "{error:#}"),
        }
        let held: Vec<_> = std::fs::read_dir(&member).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert!(held.is_empty(), "{name}: {held:?}");
    }
}

/// A node that no longer holds a snapshot it lists redirects it to the bucket's object, which the fetch reads in its
/// place, checked against the listing as the node's own bytes are: to the reader's own bucket only where it names
/// one; an object that holds other bytes than the listing names is refused, and nothing of a refused fetch stays.
#[tokio::test]
async fn a_snapshot_the_node_redirects_is_read_from_the_bucket() {
    let bucket = crate::archive::stand_in::StandIn::start().await;
    let own =
        SnapshotBucket::new(&bucket.url, &crate::daily_snapshots::snapshots_prefix(1, AccountId([2; 20]))).unwrap();
    let (origin, other) = (scratch_dir("fetch-moved-origin"), scratch_dir("fetch-moved-other"));
    let listing = publish(&origin, 5, 0);
    let elsewhere = publish(&other, 7, 0);
    let written = std::fs::read(origin.join(&listing.file)).unwrap();
    let key = |listing: &Listed| own.object(&listing.file).unwrap().path().trim_start_matches('/').to_owned();
    bucket.with(|b| b.objects.insert(key(&listing), written.clone()));
    std::fs::remove_file(origin.join(&listing.file)).unwrap();
    let url = serve_moved(&origin, own.clone()).await;
    for (case, takes) in [("own", Takes { bucket: Some(&own), this_format: true }), ("none", START)] {
        let member = scratch_dir(&format!("fetch-moved-member-{case}"));
        let path = download(&bucket.client, &url, &member, &listing, takes).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), written, "{case}");
    }
    let foreign = SnapshotBucket::new(&bucket.url, "snapshots/elsewhere/").unwrap();
    let takes = Takes { bucket: Some(&foreign), this_format: true };
    let error = refused(&bucket.client, &url, &listing, takes, "fetch-moved-foreign").await;
    assert!(error.contains("leaves the bucket [archive] names"), "{error}");
    // An object that holds other bytes than the listing names is refused, as the node's own would be.
    let mut misdirected = elsewhere.clone();
    misdirected.file = listing.file.clone();
    let error = refused(&bucket.client, &url, &misdirected, START, "fetch-moved-other-bytes").await;
    assert!(error.contains("differs from its listing"), "{error}");
}

// ── Helpers ──

/// What a member's start takes: snapshots of this build's format, from any bucket a node redirects to.
const START: Takes<'static> = Takes { bucket: None, this_format: true };

/// The error of a fetch of `listing` from `url` under `takes` into a new directory named `name`, which it leaves empty.
async fn refused(client: &reqwest::Client, url: &str, listing: &Listed, takes: Takes<'_>, name: &str) -> String {
    let member = scratch_dir(name);
    let error = download(client, url, &member, listing, takes).await.unwrap_err();
    assert!(std::fs::read_dir(&member).unwrap().next().is_none(), "{name}: nothing stays");
    format!("{error:#}")
}

/// Serves the daily snapshots in `dir` as a node's archive does, redirecting a listed one it no longer holds to its
/// object in `moved`, over plain HTTP on loopback: its base URL.
async fn serve_moved(dir: &Path, moved: SnapshotBucket) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = dir.to_owned();
    let app = Router::new().route(
        "/v1/archive/snapshots/{file}",
        get(move |axum::extract::Path(file): axum::extract::Path<String>| {
            let (path, moved) = (dir.join(&file), moved.clone());
            async move {
                match std::fs::read(path) {
                    Ok(bytes) => bytes.into_response(),
                    Err(_) => {
                        let object = moved.object(&file).unwrap().to_string();
                        (StatusCode::PERMANENT_REDIRECT, [(axum::http::header::LOCATION, object)]).into_response()
                    }
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// Publishes into `dir` a snapshot of an engine that applied ticks up to `seq`, the last stamped `ts`.
fn publish(dir: &Path, seq: u64, ts: engine_core::types::Millis) -> Listed {
    let mut engine = engine_core::Engine::default();
    for seq in 0..=seq {
        engine.apply(&engine_core::command::Sequenced { seq, ts, cmd: engine_core::command::Command::Tick }).unwrap();
    }
    let checkpoint = Checkpoint {
        parent: None,
        seq,
        root: format!("0x{}", hex::encode(engine.state_root())),
        signature: String::new(),
        signer: format!("0x{}", hex::encode([1; 20])),
        domain: None,
        deferred_withdrawals: false,
    };
    let engine = Encoding::Encoded(serde_json::to_string(&engine).unwrap());
    let offset = Offset { seq, index: 1 };
    let (carried, history) = crate::daily_snapshots::nothing_carried();
    publish_now(dir, Job { offset, checkpoint, ts, engine, carried, history, signing: Signing::for_tests() }).unwrap()
}

/// Serves the daily snapshots in `dir` as an archive does, over plain HTTP on loopback: its base URL.
async fn serve(dir: &Path) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = dir.to_owned();
    let app = Router::new().route(
        "/v1/archive/snapshots/{file}",
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
