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
    let signers = [enclave()].into();
    let take = |after, bytes: &[u8]| take_into((&mut writer, &mut copied), (DOMAIN, &signers), after, bytes);
    catch_up(&client, &followed.origins(), copied_head(&standby), take).await.expect("the copy catches up");
    drop(writer);
    assert_eq!(log_entries(&standby), log_entries(&operator)[..listed], "the standby's copy");
}

/// A copy catches up from the archive's bucket first: the segments the operator's host no longer holds come from the
/// bucket's ranges, the rest from the operator, and the copy is the operator's log.
#[tokio::test]
async fn a_copy_catches_up_from_the_bucket_first() {
    use crate::archive::origin::{bucket_of, listed_below, upload};
    let operator = scratch_dir("catch-up-bucket-operator");
    ticks(&operator, 2).await;
    let standby = scratch_dir("catch-up-bucket-standby");
    copy_log(&operator, &standby);
    ticks(&operator, 12).await;
    let view = crate::test_support::published_manifest(&operator).borrow().clone();
    let ranges = crate::archive::whole_ranges(&view).unwrap();
    let bucket = crate::archive::stand_in::StandIn::start().await;
    upload(&bucket, &operator, &view, ranges);
    let served = scratch_dir("catch-up-bucket-served");
    crate::archive::origin::copy_log(&operator, &served);
    let moved = listed_below(&operator, ranges);
    assert!(!moved.is_empty(), "the bucket holds segments the copy lacks");
    for listed in &moved {
        std::fs::remove_file(served.join(&listed.file)).unwrap();
    }
    let origins =
        crate::archive::Origins { bucket: Some(bucket_of(&bucket)), nodes: vec![serve(&served, Router::new()).await] };
    let (mut writer, mut copied) = open_copy(&standby, SEGMENT_COMMANDS, Chain::new(DOMAIN, enclave())).unwrap();
    let signers = [enclave()].into();
    let take = |after, bytes: &[u8]| take_into((&mut writer, &mut copied), (DOMAIN, &signers), after, bytes);
    catch_up(&bucket.client, &origins, copied_head(&standby), take).await.expect("the copy catches up");
    drop(writer);
    assert_eq!(log_entries(&standby), log_entries(&operator)[..completed(&operator)], "the standby's copy");
    assert!(bucket.with(|b| b.read) > 0, "the segments the operator no longer holds came from the bucket");
}

/// A member refused its stream catches up from the bucket its archive names, first: the segments the operator's host no
/// longer holds come from the bucket, which its origins alone cannot give.
#[tokio::test]
async fn a_member_catches_up_from_the_bucket_first() {
    use crate::archive::origin::{bucket_of, listed_below, upload};
    let operator = scratch_dir("catch-up-member-bucket-operator");
    ticks(&operator, 2).await;
    let member = scratch_dir("catch-up-member-bucket");
    copy_log(&operator, &member);
    ticks(&operator, 12).await;
    let view = crate::test_support::published_manifest(&operator).borrow().clone();
    let ranges = crate::archive::whole_ranges(&view).unwrap();
    let bucket = crate::archive::stand_in::StandIn::start().await;
    upload(&bucket, &operator, &view, ranges);
    let served = scratch_dir("catch-up-member-bucket-served");
    crate::archive::origin::copy_log(&operator, &served);
    for listed in &listed_below(&operator, ranges) {
        std::fs::remove_file(served.join(&listed.file)).unwrap();
    }
    let mut followed = refusing_operator(&served).await;
    followed.bucket = Some(bucket_of(&bucket));
    // The stand-in's certificate is its own authority's.
    followed.danger_accept_invalid_certs = true;
    let mut follower = Follower::open(&member, SEGMENT_COMMANDS, DOMAIN, enclave(), [enclave()].into(), None, |_| {})
        .expect("the member's copy opens");
    let head = follower.stint(&config(), &followed, Duration::from_secs(5)).await.expect("the copy catches up");
    assert_eq!(head, follower.head());
    drop(follower);
    assert_eq!(log_entries(&member), log_entries(&operator)[..completed(&operator)], "the member's copy");
    assert!(bucket.with(|b| b.read) > 0, "the segments the operator no longer holds came from the bucket");
}

/// An archive segment a key the vault never named sealed or signed, though its listing matches its bytes, is refused
/// and the copy takes nothing of it: one whose commands' seals name another signer, and one whose checkpoint another
/// key signed, as a mirror with a key of its own could serve.
#[tokio::test]
async fn an_archive_segment_another_key_signed_is_refused() {
    for (case, forge) in [("commands", Forge::Seals), ("a checkpoint", Forge::Checkpoint)] {
        let operator = scratch_dir("catch-up-forged-operator");
        ticks(&operator, 2).await;
        let copy = scratch_dir("catch-up-forged-copy");
        copy_log(&operator, &copy);
        let held = log_entries(&copy);
        ticks(&operator, 12).await;
        let manifest = Manifest::load(&operator).unwrap();
        let next = manifest.segments.iter().find(|listed| listed.first_seq > last_seq(&copy));
        forge_segment(&operator, &next.expect("a completed segment past the copy").file.clone(), forge);
        let followed = refusing_operator(&operator).await;
        let (mut writer, mut copied) = open_copy(&copy, SEGMENT_COMMANDS, Chain::new(DOMAIN, enclave())).unwrap();
        let (client, signers) = (crate::standby::archive::archive_client(false).unwrap(), [enclave()].into());
        let take = |after, bytes: &[u8]| take_into((&mut writer, &mut copied), (DOMAIN, &signers), after, bytes);
        let refused = catch_up(&client, &followed.origins(), copied_head(&copy), take).await;
        let refused = format!("{:#}", refused.err().unwrap_or_else(|| panic!("{case}: the forged segment was taken")));
        let named = ["which no checkpoint the copy holds names", "which the vault never named"];
        assert!(named.iter().any(|said| refused.contains(said)), "{case}: refused for another reason: {refused}");
        drop(writer);
        let kept = log_entries(&copy);
        assert_eq!(kept[..held.len()], held[..], "{case}: the copy keeps what it held");
        assert!(last_seq(&copy) < next_first(&operator, &held), "{case}: the copy took nothing of the forged segment");
    }
}

/// A mirror that stopped at the copy's head, though it answers, gives way to the next origin whose manifest lists
/// segments past it, from which the copy catches up.
#[tokio::test]
async fn a_mirror_that_stopped_gives_way_to_the_next_origin() {
    let operator = scratch_dir("catch-up-stale-operator");
    ticks(&operator, 2).await;
    let (standby, stale) = (scratch_dir("catch-up-stale-standby"), scratch_dir("catch-up-stale-mirror"));
    copy_log(&operator, &standby);
    copy_log(&operator, &stale);
    ticks(&operator, 12).await;
    let followed = refusing_operator_after(&operator, vec![serve(&stale, Router::new()).await]).await;
    let (mut writer, mut copied) = open_copy(&standby, SEGMENT_COMMANDS, Chain::new(DOMAIN, enclave())).unwrap();
    let (client, signers) = (crate::standby::archive::archive_client(false).unwrap(), [enclave()].into());
    let take = |after, bytes: &[u8]| take_into((&mut writer, &mut copied), (DOMAIN, &signers), after, bytes);
    catch_up(&client, &followed.origins(), copied_head(&standby), take).await.expect("the copy catches up");
    drop(writer);
    assert_eq!(log_entries(&standby), log_entries(&operator)[..completed(&operator)], "from the operator");
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
    refusing_operator_after(dir, Vec::new()).await
}

/// As `refusing_operator`, after the archive `mirrors`.
async fn refusing_operator_after(dir: &Path, mirrors: Vec<String>) -> Followed {
    let refusal = "seq 3 is before seq 4000, the first this host's log holds: read it from the log's public archive";
    let refuses = Router::new().route("/v1/stream", get(move || async move { (StatusCode::BAD_REQUEST, refusal) }));
    let standby = StandbySection {
        operator: serve(dir, refuses).await,
        mirrors,
        enclave: enclave(),
        poll_interval: Duration::from_millis(50),
        danger_accept_invalid_certs: false,
    };
    Followed::from_standby(&standby)
}

/// Serves the log in `dir` as an archive on loopback, beside `app`'s routes, and gives its URL.
async fn serve(dir: &Path, app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = dir.to_owned();
    let app = app.route(
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
    url
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
    log_entries(dir).iter().filter(|entry| matches!(entry, LogEntry::Command(_))).map(entry_seq).max().unwrap()
}

/// The offset of the last entry the copy in `dir` holds.
fn copied_head(dir: &Path) -> Option<Offset> {
    log_entries(dir).iter().try_fold(None, |last, entry| offset_of(last, entry).map(Some)).unwrap()
}

/// How a test forges an archive segment.
#[derive(Clone, Copy)]
enum Forge {
    /// Its first command is sealed, validly over the link it continues, by another key.
    Seals,
    /// Its first checkpoint is signed by another key, named as its signer.
    Checkpoint,
}

/// Forges the segment `file` in `dir` as `forge` says, and lists its new bytes, so the archive serves it whole and
/// true to its listing.
fn forge_segment(dir: &Path, file: &str, forge: Forge) {
    use crate::log::command_seal::{checkpoint_link, command_link, seal_digest};
    use sha2::Digest;
    let (other, path) = (EnclaveKey::generate(), dir.join(file));
    // The link the segment's first command continues: the chain restarts at each checkpoint.
    let first = Manifest::load(dir).unwrap().segments.iter().find(|listed| listed.file == file).unwrap().first_seq;
    let mut link = engine_core::eip712::B256::default();
    for entry in
        log_entries(dir).iter().take_while(|entry| !matches!(entry, LogEntry::Command(c) if c.sequenced.seq == first))
    {
        link = match entry {
            LogEntry::Command(command) => command_link(link, command).unwrap(),
            LogEntry::Checkpoint(checkpoint) => checkpoint_link(checkpoint).unwrap(),
            LogEntry::Withdrawal(_) => link,
        };
    }
    let mut forged = false;
    let lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut entry: LogEntry = serde_json::from_str(line).unwrap();
            match (&mut entry, forge) {
                (LogEntry::Command(command), Forge::Seals) if !forged => {
                    let sealed = seal_digest(DOMAIN, command_link(link, command).unwrap());
                    let seal = command.seal.as_mut().expect("the operator seals its commands");
                    (seal.signer, seal.signature) =
                        (engine_core::types::AccountId(other.address()), other.sign_digest(sealed));
                    forged = true;
                }
                (LogEntry::Checkpoint(checkpoint), Forge::Checkpoint) if !forged => {
                    let root: [u8; 32] = checkpoint.root.parse::<engine_core::eip712::B256>().unwrap().0;
                    let CheckpointDomain { chain_id, vault } = DOMAIN;
                    checkpoint.signature = other.sign_checkpoint(chain_id, &vault.0, checkpoint.seq, &root);
                    checkpoint.signer = other.address_hex();
                    forged = true;
                }
                _ => {}
            }
            serde_json::to_string(&entry).unwrap()
        })
        .collect();
    assert!(forged, "the segment {file} holds nothing to forge");
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

/// The first seq of the operator's segment in `dir` after the last command `held` holds.
fn next_first(dir: &Path, held: &[LogEntry]) -> u64 {
    let last = held.iter().filter(|entry| matches!(entry, LogEntry::Command(_))).map(entry_seq).max();
    let manifest = Manifest::load(dir).unwrap();
    manifest.segments.iter().map(|listed| listed.first_seq).find(|first| Some(*first) > last).unwrap()
}
