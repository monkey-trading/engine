//! The archive read's dev scenario (`scripts/dev-archive-read.sh`): a dev node uploads its log's ranges to a
//! loopback stand-in for the bucket as a stand-in vault finalizes them, and its older segments leave its disk. Every
//! reader then reads the whole log from the bucket and the node: the node's archive redirects to the bucket, `engine
//! replay` and `engine exit-proof` read the log from the node's URL as from its disk, a member backfills an empty
//! copy, and an object the bucket serves otherwise is taken from the node, or refused where the node no longer has it.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::net::TcpListener;

use crate::archive::read::origin::bucket_of;
use crate::archive::scenario::{finalize_all, ticks_until, uploading};
use crate::archive::stand_in::{StandIn, Vault, test_log};
use crate::archive::{Origins, compress, download, range_bytes, range_key, range_seqs};
use crate::exit::{ExitSubject, exit_bundle};
use crate::log::{MANIFEST, SegmentInfo};
use crate::sequencer::{Replayed, replay_path_in_domain};
use crate::standby::archive::bootstrap_archive;
use crate::test_support::{TRADER, TestSequencer, deposit, log_entries, published_manifest, scratch_dir};

#[tokio::test]
async fn dev_scenario_archive_read() {
    let (bucket, dir, vault) = (StandIn::start().await, scratch_dir("archive-read-scenario"), Vault::default());

    println!(
        "==> 1. a dev node writes flow, and its uploader puts each range below the vault's finality in the bucket"
    );
    let node = TestSequencer::start(&dir, 256);
    node.submit(deposit(1_000, "archive-read-deposit")).await;
    ticks_until(&node, 6).await;
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), &vault);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let archived = finalize_all(&node, &dir, &vault, &metrics).await;
    running.abort();
    ticks_until(&node, archived + 2).await;
    let manifest = node.manifest.clone();
    node.stop().await;
    let listed = manifest.borrow().segments.clone();
    let local = completed_copy(&dir, &listed, "archive-read-scenario-local");
    println!("    {archived} ranges in the bucket, {} completed segments", listed.len());

    println!(
        "==> 2. the segments the bucket's first ranges hold leave the node's disk, and its archive redirects them"
    );
    let kept_from = archived - 1;
    let moved: Vec<SegmentInfo> = listed.iter().filter(|s| s.last_seq < kept_from * range_seqs()).cloned().collect();
    for segment in &moved {
        std::fs::remove_file(dir.join(&segment.file)).unwrap();
    }
    let url = serve_archive(manifest, &dir, &bucket).await;
    let first = &moved[0];
    let answer = bucket.client.get(format!("{url}/v1/archive/{}", first.file)).send().await.unwrap();
    assert_eq!(answer.status(), 308);
    let location = answer.headers()["location"].to_str().unwrap().to_owned();
    assert_eq!(location, bucket_of(&bucket).object(first.first_seq / range_seqs()).unwrap().as_str());
    let mut reader = crate::archive::Segments::new(&bucket.client, std::slice::from_ref(&url));
    let rebuilt = reader.segment(first).await.unwrap();
    assert_eq!(rebuilt.len() as u64, first.bytes, "the rebuilt segment is its listing's length and hash");
    println!("    {} segments moved; {} answers 308 to {location}", moved.len(), first.file);

    println!("==> 3. engine replay --log <the node's URL> reaches the roots, commands and signers of the local replay");
    let downloaded = scratch_dir("archive-read-scenario-downloaded");
    download(&bucket.client, &url.parse().unwrap(), &downloaded).await.unwrap();
    let (from_url, from_disk) = (summary(replay(&downloaded)), summary(replay(&local)));
    assert_eq!(from_url, from_disk);
    println!("    {} commands, {} checkpoints, root {}", from_url.0, from_url.1, from_url.2);

    println!(
        "==> 4. a member with an empty copy backfills it, its older segments from the bucket, its newest from the node"
    );
    let reads = bucket.with(|b| b.read);
    let copy = scratch_dir("archive-read-scenario-member");
    let origins = Origins { bucket: Some(bucket_of(&bucket)), nodes: vec![url.clone()] };
    bootstrap_archive(&bucket.client, &origins, &copy).await.unwrap();
    assert_eq!(log_entries(&copy), log_entries(&local));
    assert_eq!(bucket.with(|b| b.read) - reads, archived + 1, "each range once, and the first it lacks");
    let past = listed.iter().filter(|s| s.last_seq >= archived * range_seqs()).count();
    println!("    {} segments from the bucket's {archived} ranges, {past} from the node", listed.len() - past);

    println!("==> 5. the bucket serves other bytes for an object: the backfill takes its segments from the node, and");
    println!("       engine replay refuses one the node no longer holds, naming it");
    alter(&bucket, &local, kept_from);
    let other = scratch_dir("archive-read-scenario-member-again");
    bootstrap_archive(&bucket.client, &origins, &other).await.unwrap();
    assert_eq!(log_entries(&other), log_entries(&local), "the segments the node holds come from it");
    alter(&bucket, &local, 0);
    let refused = scratch_dir("archive-read-scenario-refused");
    let e = download(&bucket.client, &url.parse().unwrap(), &refused).await.unwrap_err();
    assert!(format!("{e:#}").contains(&format!("archive segment {} differs from its listing", first.file)), "{e:#}");
    println!("    refused: {e:#}");

    println!("==> 6. engine exit-proof --log <the node's URL> prints the proofs it prints from the local log");
    let seq = last_checkpoint(&downloaded);
    let proofs = |path: &Path| serde_json::to_string(&exit_bundle(path, seq, ExitSubject::Account(TRADER)).unwrap());
    assert_eq!(proofs(&downloaded).unwrap(), proofs(&local).unwrap());
    println!("    the same proofs for {TRADER} at seq {seq}");
    println!("PASS: the whole log reads from the bucket and the node, as from the node's disk");
}

// ── Helpers ──

/// Copies the completed segments `listed` of the log in `dir`, and their listings, into a new directory named for
/// `name`: the log a reader of the archive reads.
fn completed_copy(dir: &Path, listed: &[SegmentInfo], name: &str) -> std::path::PathBuf {
    let copy = scratch_dir(name);
    std::fs::create_dir_all(&copy).unwrap();
    let mut manifest = String::new();
    for segment in listed {
        std::fs::copy(dir.join(&segment.file), copy.join(&segment.file)).unwrap();
        manifest.push_str(&format!("{}\n", serde_json::to_string(segment).unwrap()));
    }
    std::fs::write(copy.join(MANIFEST), manifest).unwrap();
    copy
}

/// Serves the archive of the log in `dir`, whose writer published `manifest`, from the node's own route, its moved
/// segments redirected to `bucket`.
async fn serve_archive(
    manifest: tokio::sync::watch::Receiver<crate::log::Manifest>,
    dir: &Path,
    bucket: &StandIn,
) -> String {
    let (mut state, _requests) = crate::api::tests::api_state(true);
    (state.log_dir, state.manifest, state.archive_objects) = (dir.to_owned(), manifest, Some(bucket_of(bucket)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::api::router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

/// Has `bucket` serve range `index` of the log in `dir` with one timestamp changed, as an object holding other
/// bytes than the log does.
fn alter(bucket: &StandIn, dir: &Path, index: u64) {
    let view = published_manifest(dir).borrow().clone();
    let range = String::from_utf8(range_bytes(dir, &view, index).unwrap()).unwrap();
    let altered = range.replacen("\"ts\":1", "\"ts\":2", 1);
    assert_ne!(altered, range);
    bucket.with(|b| b.served.insert(range_key(&test_log(), index), compress(altered.as_bytes()).unwrap()));
}

/// The replay of the log at `path`, as `engine replay` replays it with no pinned signer.
fn replay(path: &Path) -> Replayed {
    replay_path_in_domain(path, None).unwrap()
}

/// What a replay's summary prints: its commands, checkpoints, final root and checkpoint signers.
fn summary(mut replayed: Replayed) -> (u64, u64, String, Vec<String>) {
    let signers = replayed.checkpoint_signers.iter().map(ToString::to_string).collect();
    (replayed.commands, replayed.checkpoints, hex::encode(replayed.engine.state_root()), signers)
}

/// The seq of the last checkpoint in the log at `path`.
fn last_checkpoint(path: &Path) -> u64 {
    log_entries(path)
        .into_iter()
        .filter_map(|entry| match entry {
            crate::log::LogEntry::Checkpoint(checkpoint) => Some(checkpoint.seq),
            _ => None,
        })
        .next_back()
        .expect("a checkpoint")
}
