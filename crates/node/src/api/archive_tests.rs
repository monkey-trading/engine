//! The daily snapshots the archive serves beside the log's segments.

use std::fs;
use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::{Path as FilePath, State};
use axum::http::header::LOCATION;
use engine_core::Engine;
use engine_core::command::{Command, Sequenced};
use tokio::sync::Semaphore;

use crate::api::tests::{api_state, get, serve_state};
use crate::api::{ApiError, ServedSnapshots};
use crate::daily_snapshots::{Job, MANIFEST, Signing, SnapshotBucket, publish_now, publish_ticks};
use crate::log::{Checkpoint, Offset};
use crate::snapshot::sealing::Encoding;
use crate::test_support::scratch_dir;

/// The archive serves the daily snapshots' manifest and each snapshot it lists, as written, with no backfill
/// permit, and answers 404 for any other name, or when the node publishes none, and 500 for a snapshot that
/// differs from its listing.
#[tokio::test]
async fn the_archive_serves_the_daily_snapshots_it_lists_as_written() {
    let dir = scratch_dir("archive-daily");
    let (mut state, _requests) = api_state(true);
    let unpublished = serve_state(state.clone()).await;
    assert_eq!(get(unpublished, &format!("/v1/archive/snapshots/{MANIFEST}")).await.0, 404);
    state.daily_snapshots = Some(Arc::new(ServedSnapshots::new(dir.clone(), None)));
    // A snapshot's read takes none of the stream's backfills.
    state.backfills = Arc::new(Semaphore::new(0));
    let addr = serve_state(state).await;
    assert_eq!(get(addr, &format!("/v1/archive/snapshots/{MANIFEST}")).await, (200, Vec::new()));

    let listing = publish_now(&dir, tick_snapshot()).unwrap();
    let written = fs::read(dir.join(&listing.file)).unwrap();
    assert_eq!(
        get(addr, &format!("/v1/archive/snapshots/{MANIFEST}")).await,
        (200, fs::read(dir.join(MANIFEST)).unwrap())
    );
    assert_eq!(get(addr, &format!("/v1/archive/snapshots/{}", listing.file)).await, (200, written.clone()));
    fs::write(dir.join("00000000000000000009-0000000001.json"), &written).unwrap();
    for path in ["00000000000000000009-0000000001.json", "%2Fetc%2Fhosts", "nothing.json"] {
        assert_eq!(get(addr, &format!("/v1/archive/snapshots/{path}")).await.0, 404, "{path}");
    }
    let mut damaged = written;
    damaged[10] ^= 1;
    fs::write(dir.join(&listing.file), damaged).unwrap();
    assert_eq!(get(addr, &format!("/v1/archive/snapshots/{}", listing.file)).await.0, 500);
}

/// A listed snapshot the host no longer holds is redirected (`308`) to its object in the bucket, relaying none of it,
/// while one it holds is served from the handle its check read; a node that names no bucket answers 500 for it, and
/// a name the manifest does not list is not redirected.
#[tokio::test]
async fn a_listed_snapshot_the_host_no_longer_holds_redirects_to_its_object() {
    let dir = scratch_dir("archive-daily-moved");
    let (gone, held) = (publish_ticks(&dir, 3, 1_000), publish_ticks(&dir, 5, 2_000));
    fs::remove_file(dir.join(&gone.file)).unwrap();
    let ask = |moved: Option<SnapshotBucket>, file: String| {
        let (mut state, _requests) = api_state(true);
        state.daily_snapshots = Some(Arc::new(ServedSnapshots::new(dir.clone(), moved)));
        crate::api::archive::snapshot(State(state), FilePath(file))
    };
    assert!(matches!(ask(None, gone.file.clone()).await, Err(ApiError::Internal)), "no bucket keeps it");
    let bucket = SnapshotBucket::new(&"https://archive.example".parse().unwrap(), "snapshots/31337/0x00/").unwrap();
    let moved = ask(Some(bucket.clone()), gone.file.clone()).await.expect("a redirect");
    assert_eq!(moved.status(), 308);
    assert_eq!(moved.headers()[LOCATION], bucket.object(&gone.file).unwrap().as_str());
    assert!(to_bytes(moved.into_body(), usize::MAX).await.unwrap().is_empty(), "nothing relayed");
    let served = ask(Some(bucket.clone()), held.file.clone()).await.expect("a held snapshot");
    assert_eq!(to_bytes(served.into_body(), usize::MAX).await.unwrap(), fs::read(dir.join(&held.file)).unwrap());
    let unlisted = "00000000000000000009-0000000001.json".to_owned();
    assert!(matches!(ask(Some(bucket), unlisted).await, Err(ApiError::NotFound)), "an unlisted name");
}

/// A manifest line under a name its offset does not give, as a host could write over the manifest, with the length and
/// SHA-256 of a file it names, inside the snapshot directory or climbing out of it, is not served: `404`, while the
/// canonical listing beside it is.
#[tokio::test]
async fn a_listing_under_another_name_than_its_offsets_is_not_served() {
    let (dir, outside) = (scratch_dir("archive-daily-names"), scratch_dir("archive-daily-names-outside"));
    let listing = publish_ticks(&dir, 3, 1_000);
    let written = fs::read(dir.join(&listing.file)).unwrap();
    let mut renamed = listing.clone();
    renamed.file = "renamed.json".to_owned();
    let mut climbing = listing.clone();
    climbing.file = format!("../{}/{}", outside.file_name().unwrap().to_string_lossy(), listing.file);
    fs::create_dir_all(&outside).unwrap();
    for named in [&renamed, &climbing] {
        fs::write(dir.join(&named.file), &written).unwrap();
    }
    crate::daily_snapshots::write_listing(&dir, &[listing.clone(), renamed.clone(), climbing.clone()]);
    let ask = |file: String| {
        let (mut state, _requests) = api_state(true);
        state.daily_snapshots = Some(Arc::new(ServedSnapshots::new(dir.clone(), None)));
        crate::api::archive::snapshot(State(state), FilePath(file))
    };
    let served = ask(listing.file.clone()).await.expect("the canonical listing");
    assert_eq!(to_bytes(served.into_body(), usize::MAX).await.unwrap(), written);
    for named in [renamed, climbing] {
        assert!(matches!(ask(named.file.clone()).await, Err(ApiError::NotFound)), "{}", named.file);
    }
}

/// The node's archive serves the snapshots `[daily_snapshots]` names, and with `[archive]` redirects one the host no
/// longer holds to its object under the custody chain and vault; without `[archive]` it redirects none.
#[tokio::test]
async fn the_node_redirects_its_snapshots_to_the_bucket_its_archive_names() {
    let mut cfg = crate::test_support::dev_config();
    assert!(ServedSnapshots::for_config(&cfg).unwrap().is_none(), "no section");
    let dir = scratch_dir("archive-daily-config");
    let gone = publish_ticks(&dir, 3, 1_000);
    fs::remove_file(dir.join(&gone.file)).unwrap();
    cfg.daily_snapshots = Some(crate::config::DailySnapshotsSection::at(dir));
    let ask = |served: Arc<ServedSnapshots>| {
        let (mut state, _requests) = api_state(true);
        state.daily_snapshots = Some(served);
        crate::api::archive::snapshot(State(state), FilePath(gone.file.clone()))
    };
    let unarchived = ServedSnapshots::for_config(&cfg).unwrap().expect("the section");
    assert!(matches!(ask(unarchived).await, Err(ApiError::Internal)), "no bucket keeps it");
    cfg.archive = Some(
        toml::from_str(
            "endpoint = 'https://account.r2.cloudflarestorage.com'\nbucket = 'engine-log'\nregion = 'auto'\n\
             public_url = 'https://archive.example.com'\naccess_key_id_secret = 'archive_access_key_id'\n\
             secret_access_key_secret = 'archive_secret_access_key'\n",
        )
        .unwrap(),
    );
    let moved = ask(ServedSnapshots::for_config(&cfg).unwrap().expect("the section")).await.expect("a redirect");
    let vault = cfg.custody.vault.to_lowercase();
    let object = format!("https://archive.example.com/snapshots/{}/{vault}/{}", cfg.custody.chain_id, gone.file);
    assert_eq!((moved.status().as_u16(), moved.headers()[LOCATION].to_str().unwrap()), (308, object.as_str()));
}

// ── Helpers ──

/// A snapshot of an engine that applied one tick, at its checkpoint.
fn tick_snapshot() -> Job {
    let mut engine = Engine::default();
    engine.apply(&Sequenced { seq: 0, ts: 0, cmd: Command::Tick }).unwrap();
    let checkpoint = Checkpoint {
        parent: None,
        seq: 0,
        root: format!("0x{}", hex::encode(engine.state_root())),
        signature: String::new(),
        signer: format!("0x{}", hex::encode([1; 20])),
        domain: None,
        deferred_withdrawals: false,
    };
    let engine = Encoding::Encoded(serde_json::to_string(&engine).unwrap());
    let (carried, history) = crate::daily_snapshots::nothing_carried();
    Job {
        offset: Offset { seq: 0, index: 1 },
        checkpoint,
        ts: 0,
        engine,
        carried,
        history,
        signing: Signing::for_tests(),
    }
}
