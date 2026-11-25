//! A follower whose copy trails what the operator serves its stream from: the operator's refusal names the archive,
//! and the follower takes the entries after its head from the archive's completed segments, each checked as a
//! stream's entry is, a member's replica applying them, then follows on from its new head.

use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::net::TcpListener;

use super::*;
use crate::config::{NodeConfig, StandbySection};
use crate::log::{CheckpointDomain, Manifest};
use crate::standby::tests::{DOMAIN, copy_log, ticks};
use crate::standby::{Followed, Follower, open_copy};
use crate::tee::EnclaveKey;
use crate::test_support::{SEGMENT_COMMANDS, dev_config, log_entries, scratch_dir};

/// A member refused its stream from its head catches up from the archive inside its stint, its replica applying every
/// entry its copy takes, to the end of the operator's last completed segment; a standby's copy, with no replica,
/// catches up the same way.
#[tokio::test]
async fn a_follower_refused_its_stream_catches_up_from_the_archive() {
    let operator = scratch_dir("catch-up-operator");
    ticks(&operator, 2).await;
    let (member, standby) = (scratch_dir("catch-up-member"), scratch_dir("catch-up-standby"));
    copy_log(&operator, &member);
    copy_log(&operator, &standby);
    ticks(&operator, 12).await;
    let followed = refusing_operator(&operator).await;
    let listed = completed(&operator);
    assert!(listed > log_entries(&member).len(), "the operator completed segments past the copies");

    let mut follower = Follower::open(&member, SEGMENT_COMMANDS, DOMAIN, enclave(), [enclave()].into(), None, |_| {})
        .expect("the member's copy opens");
    let head = follower.stint(&config(), &followed, Duration::from_secs(5)).await.expect("the copy catches up");
    assert_eq!(head, follower.head());
    assert_eq!(follower.replica().last_seq(), head.map(|head| head.seq), "the replica applied every entry");
    drop(follower);
    assert_eq!(log_entries(&member), log_entries(&operator)[..listed], "the member's copy");

    let (mut writer, mut copied) = open_copy(&standby, SEGMENT_COMMANDS, Chain::new(DOMAIN, enclave())).unwrap();
    let client = crate::standby::archive::archive_client(false).unwrap();
    let take = |after, bytes: &[u8]| take_into(&mut writer, &mut copied, after, bytes);
    catch_up(&client, &followed.origins(), copied_head(&standby), take).await.expect("the copy catches up");
    drop(writer);
    assert_eq!(log_entries(&standby), log_entries(&operator)[..listed], "the standby's copy");
}

/// An archive segment that does not extend the copy, here one whose commands' seals name another signer though its
/// listing matches its bytes, is refused, and the copy takes none of it.
#[tokio::test]
async fn an_archive_segment_that_does_not_extend_the_copy_is_refused() {
    let operator = scratch_dir("catch-up-forged-operator");
    ticks(&operator, 2).await;
    let copy = scratch_dir("catch-up-forged-copy");
    copy_log(&operator, &copy);
    let held = log_entries(&copy).len();
    ticks(&operator, 12).await;
    let manifest = Manifest::load(&operator).unwrap();
    let next = manifest.segments.iter().find(|listed| listed.first_seq > last_seq(&copy));
    reseal(&operator, &next.expect("a completed segment past the copy").file.clone());
    let followed = refusing_operator(&operator).await;
    let (mut writer, mut copied) = open_copy(&copy, SEGMENT_COMMANDS, Chain::new(DOMAIN, enclave())).unwrap();
    let client = crate::standby::archive::archive_client(false).unwrap();
    let take = |after, bytes: &[u8]| take_into(&mut writer, &mut copied, after, bytes);
    let refused = catch_up(&client, &followed.origins(), copied_head(&copy), take).await;
    assert!(refused.is_err(), "the forged segment was taken");
    drop(writer);
    assert!(log_entries(&copy).len() >= held, "the copy keeps what it held");
    assert!(last_seq(&copy) < last_seq(&operator), "the copy took nothing of the forged segment on");
}

/// A stream refused for a start before what the operator's start verified is told apart from any other failure.
#[tokio::test]
async fn the_refusal_naming_the_archive_is_told_apart() {
    let operator = scratch_dir("catch-up-refusal");
    ticks(&operator, 1).await;
    let copy = scratch_dir("catch-up-refusal-copy");
    copy_log(&operator, &copy);
    let followed = refusing_operator(&operator).await;
    let (mut writer, mut copied) = open_copy(&copy, SEGMENT_COMMANDS, Chain::new(DOMAIN, enclave())).unwrap();
    let refused = crate::standby::follow::follow(
        &mut writer,
        &mut copied,
        &config(),
        &followed,
        Duration::from_secs(5),
        None,
        None,
    )
    .await
    .expect_err("the stream is refused");
    assert!(before_served(&refused), "{refused:#}");
    assert!(!before_served(&anyhow::anyhow!("connecting failed")));
}

// ── Helpers ──

/// The test sequencer's enclave, which seals the operator's log.
fn enclave() -> engine_core::types::AccountId {
    engine_core::types::AccountId(EnclaveKey::test_key().address())
}

/// The dev config on the test sequencer's deployment.
fn config() -> NodeConfig {
    let mut cfg = dev_config();
    let CheckpointDomain { chain_id, vault } = DOMAIN;
    (cfg.custody.chain_id, cfg.custody.vault) = (chain_id, format!("0x{}", hex::encode(vault.0)));
    cfg
}

/// A stand-in operator, on loopback, that started from a daily snapshot past every copy: it refuses every stream
/// with the 400 that names the archive, and serves its log in `dir` as its archive.
async fn refusing_operator(dir: &Path) -> Followed {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = dir.to_owned();
    let refusal = "seq 3 is before seq 4000, the first this host's log holds: read it from the log's public archive";
    let app = Router::new().route("/v1/stream", get(move || async move { (StatusCode::BAD_REQUEST, refusal) })).route(
        "/v1/archive/{file}",
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
    let standby = StandbySection {
        operator: url,
        mirrors: Vec::new(),
        enclave: enclave(),
        poll_interval: Duration::from_millis(50),
        danger_accept_invalid_certs: false,
    };
    Followed::from_standby(&standby)
}

/// How many entries the completed segments the manifest in `dir` lists hold.
fn completed(dir: &Path) -> usize {
    let listed = Manifest::load(dir).unwrap().segments.iter().map(|listed| listed.last_seq).max().unwrap();
    log_entries(dir).iter().take_while(|entry| entry_seq(entry) <= listed).count()
}

/// The seq of `entry`, the command's or the one it follows.
fn entry_seq(entry: &LogEntry) -> u64 {
    match entry {
        LogEntry::Command(command) => command.sequenced.seq,
        LogEntry::Checkpoint(checkpoint) => checkpoint.seq,
        LogEntry::Withdrawal(_) => u64::MAX,
    }
}

/// The seq of the last command the log in `dir` holds.
fn last_seq(dir: &Path) -> u64 {
    log_entries(dir)
        .iter()
        .filter_map(|entry| matches!(entry, LogEntry::Command(_)).then(|| entry_seq(entry)))
        .max()
        .unwrap()
}

/// The offset of the last entry the copy in `dir` holds.
fn copied_head(dir: &Path) -> Option<Offset> {
    log_entries(dir).iter().try_fold(None, |last, entry| offset_of(last, entry).map(Some)).unwrap()
}

/// Names another signer on the seal of each command of the segment `file` in `dir`, and lists its new bytes, so the
/// archive serves it whole and true to its listing.
fn reseal(dir: &Path, file: &str) {
    use sha2::Digest;
    let (other, path) = (EnclaveKey::generate(), dir.join(file));
    let lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut entry: LogEntry = serde_json::from_str(line).unwrap();
            if let LogEntry::Command(command) = &mut entry
                && let Some(seal) = &mut command.seal
            {
                seal.signer = engine_core::types::AccountId(other.address());
            }
            serde_json::to_string(&entry).unwrap()
        })
        .collect();
    let bytes = lines.join("\n") + "\n";
    std::fs::write(&path, &bytes).unwrap();
    let manifest = Manifest::load(dir).unwrap();
    let text: String = manifest
        .segments
        .iter()
        .cloned()
        .map(|mut listed| {
            if listed.file == file {
                (listed.bytes, listed.sha256) = (bytes.len() as u64, hex::encode(sha2::Sha256::digest(&bytes)));
            }
            serde_json::to_string(&listed).unwrap() + "\n"
        })
        .collect();
    std::fs::write(dir.join(crate::log::MANIFEST), text).unwrap();
}
