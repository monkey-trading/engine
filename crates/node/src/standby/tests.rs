//! The standby's copy: following verifies frames and the operator's chain before appending entries,
//! and the takeover truncates the copy at the vault's latest checkpoint.

use std::time::Duration;

use engine_core::command::{Command, Sequenced};
use engine_wire::{Connection, Deployment, FRAME_VERSION, SignedFrame};
use futures_util::SinkExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::log::{Checkpoint, CommandEntry, LogEntry, Offset, SegmentedLog, truncate_to_checkpoint};
use crate::tee::EnclaveKey;
use crate::test_support::{TestSequencer, dev_config, log_entries, log_messages, scratch_dir};

fn command(seq: u64) -> LogEntry {
    LogEntry::Command(CommandEntry {
        sequenced: Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick },
        events: Some(Vec::new()),
        seal: None,
    })
}

fn checkpoint(seq: u64, root: u8) -> LogEntry {
    LogEntry::Checkpoint(Checkpoint {
        parent: None,
        seq,
        root: format!("0x{}", hex::encode([root; 32])),
        signature: format!("0x{}", "00".repeat(65)),
        signer: format!("0x{}", "11".repeat(20)),
        domain: None,
        deferred_withdrawals: false,
    })
}

fn open_writer(dir: &std::path::Path, segment_commands: u64) -> LogWriter {
    let (manifest, _) = tokio::sync::watch::channel(Manifest::default());
    SegmentedLog::open(dir, |_| Ok(())).unwrap().into_writer(segment_commands, manifest).unwrap()
}

fn entries(dir: &std::path::Path) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    SegmentedLog::open(dir, |entry| {
        entries.push(entry);
        Ok(())
    })
    .unwrap();
    entries
}

fn root(root: u8) -> String {
    format!("0x{}", hex::encode([root; 32]))
}

#[test]
fn truncate_drops_the_open_tail() {
    let dir = scratch_dir("standby-truncate-tail");
    let mut writer = open_writer(&dir, 100);
    writer.append(&command(0)).unwrap();
    writer.append(&checkpoint(0, 7)).unwrap();
    writer.append(&command(1)).unwrap();
    writer.flush().unwrap();
    drop(writer);
    truncate_to_checkpoint(&dir, 0, &root(7)).unwrap();
    assert_eq!(entries(&dir), vec![command(0), checkpoint(0, 7)]);
}

#[test]
fn truncate_removes_later_segments_and_keeps_a_listing_the_checkpoint_ends() {
    let dir = scratch_dir("standby-truncate-listed");
    let mut writer = open_writer(&dir, 1);
    for entry in [command(0), checkpoint(0, 7), command(1), checkpoint(1, 8)] {
        writer.append(&entry).unwrap();
    }
    writer.flush().unwrap();
    drop(writer);
    // `command(1)` rotated the segment holding checkpoint 0, which ends it: its listing stays.
    truncate_to_checkpoint(&dir, 0, &root(7)).unwrap();
    assert_eq!(entries(&dir), vec![command(0), checkpoint(0, 7)]);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "one segment and the manifest");
}

#[test]
fn truncate_mid_segment_drops_its_listing() {
    let dir = scratch_dir("standby-truncate-mid");
    let mut writer = open_writer(&dir, 3);
    for entry in [command(0), command(1), checkpoint(1, 7), command(2), command(3)] {
        writer.append(&entry).unwrap();
    }
    writer.flush().unwrap();
    drop(writer);
    // Checkpoint 1 sits inside the listed first segment, not at its end: the segment is trimmed
    // and becomes the unlisted open one.
    truncate_to_checkpoint(&dir, 1, &root(7)).unwrap();
    assert_eq!(entries(&dir), vec![command(0), command(1), checkpoint(1, 7)]);
    let manifest = std::fs::read_to_string(dir.join(crate::log::MANIFEST)).unwrap();
    assert!(manifest.is_empty(), "the trimmed segment's listing is dropped: {manifest}");
}

#[test]
fn truncate_removes_later_listed_segments() {
    // The accepted checkpoint ends a completed, listed segment with another listed segment after
    // it: the later file and its listing both go, and the copy still opens.
    let dir = scratch_dir("standby-truncate-listed-later");
    let mut writer = open_writer(&dir, 1);
    for entry in [command(0), checkpoint(0, 7), command(1), checkpoint(1, 8), command(2), checkpoint(2, 9)] {
        writer.append(&entry).unwrap();
    }
    writer.flush().unwrap();
    drop(writer);
    truncate_to_checkpoint(&dir, 0, &root(7)).unwrap();
    assert_eq!(entries(&dir), vec![command(0), checkpoint(0, 7)]);
    let manifest = std::fs::read_to_string(dir.join(crate::log::MANIFEST)).unwrap();
    assert_eq!(manifest.lines().count(), 1, "only the target's listing stays: {manifest}");
}

#[test]
fn archive_listings_must_be_canonical_and_ordered() {
    use crate::log::SegmentInfo;
    let listed = |file: &str, first_seq| SegmentInfo {
        file: file.into(),
        first_seq,
        last_seq: first_seq,
        bytes: 1,
        sha256: "00".repeat(32),
    };
    assert!(super::archive::validate_listing(&listed("00000000000000000000.jsonl", 0)).is_ok());
    assert!(super::archive::validate_listing(&listed("../escape.jsonl", 0)).is_err(), "a traversal name is refused");
    assert!(super::archive::validate_listing(&listed("/abs/path.jsonl", 0)).is_err(), "an absolute name is refused");
    assert!(
        super::archive::validate_listing(&listed("00000000000000000001.jsonl", 0)).is_err(),
        "a wrong seq name is refused"
    );
}

#[test]
fn a_listing_or_segment_failing_its_check_is_an_integrity_failure() {
    use super::archive::{ArchiveIntegrity, check_listing, validate_listing};
    let listed = crate::log::SegmentInfo {
        file: "00000000000000000000.jsonl".into(),
        first_seq: 0,
        last_seq: 0,
        bytes: 3,
        sha256: super::sha256(b"abc"),
    };
    check_listing(&listed, b"abc").unwrap();
    for (what, err) in [
        ("other bytes", check_listing(&listed, b"abd").unwrap_err()),
        (
            "a name for another seq",
            validate_listing(&crate::log::SegmentInfo { first_seq: 1, ..listed.clone() }).unwrap_err(),
        ),
    ] {
        assert!(err.downcast_ref::<ArchiveIntegrity>().is_some(), "{what}: {err:#}");
    }
}

#[test]
fn truncate_refuses_a_checkpoint_the_copy_lacks() {
    let dir = scratch_dir("standby-truncate-missing");
    let mut writer = open_writer(&dir, 100);
    writer.append(&command(0)).unwrap();
    writer.flush().unwrap();
    drop(writer);
    assert!(truncate_to_checkpoint(&dir, 9, &root(7)).is_err(), "a missing checkpoint truncated nothing");
}

#[test]
fn stream_url_rewrites_http_schemes() {
    assert_eq!(
        stream_url("https://operator.example.com/", "/v1/stream?signed=1").unwrap(),
        "wss://operator.example.com/v1/stream?signed=1"
    );
    assert_eq!(stream_url("http://127.0.0.1:8080", "/v1/stream").unwrap(), "ws://127.0.0.1:8080/v1/stream");
    assert!(stream_url("operator.example.com", "/v1/stream").is_err());
}

/// One signed frame as the operator's stream serves it.
fn frame(
    key: &EnclaveKey,
    deployment: &Deployment,
    connection: &Connection,
    n: u64,
    previous: &mut [u8; 32],
    msg: String,
) -> String {
    let time = 1_700_000_000_000u64;
    let digest = engine_wire::frame_digest(deployment, connection, previous, n, time, &msg);
    let sig = key.sign_digest(digest);
    *previous = digest;
    serde_json::to_string(&SignedFrame { v: FRAME_VERSION, n, time, msg, sig }).unwrap()
}

#[tokio::test]
async fn follow_appends_verified_frames_and_refuses_a_wrong_signer() {
    let operator = scratch_dir("standby-follow-operator");
    ticks(&operator, 2).await;
    let messages = log_messages(&operator, Offset::start_of(0));
    let mut frames: Vec<_> = messages.iter().take(3).map(|message| (By::Operator, message.clone())).collect();
    // One frame from another key: the standby must fail the connection, not append it.
    frames.push((By::Other, messages[3].clone()));
    let (port, server, _) = operator_stream(frames).await;

    let dir = scratch_dir("standby-follow");
    let (mut writer, mut copied) = open_copy(&dir, 100, Chain::new(DOMAIN, enclave())).unwrap();
    let err = follow(
        &mut writer,
        &mut copied,
        &config(),
        &Followed::from_standby(&standby_section(port)),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("failed verification"), "{err:#}");
    assert!(rollback_of(&err).is_none(), "a frame that fails verification is not the operator's log: {err:#}");
    drop(writer);
    assert_eq!(entries(&dir), log_entries(&operator)[..3]);
    server.abort();
}

#[tokio::test]
async fn follow_resumes_at_the_copys_last_command() {
    let dir = scratch_dir("standby-follow-resume");
    let mut writer = open_writer(&dir, 100);
    writer.append(&command(0)).unwrap();
    writer.append(&checkpoint(0, 5)).unwrap();
    writer.flush().unwrap();
    drop(writer);
    let (mut writer, mut copied) = open_copy(&dir, 100, Chain::new(DOMAIN, enclave())).unwrap();
    let (port, server, request) = operator_stream(Vec::new()).await;
    // The host held the connection open and fed it nothing: that is a failed stint, not a working one.
    let failed = follow(
        &mut writer,
        &mut copied,
        &config(),
        &Followed::from_standby(&standby_section(port)),
        Duration::from_millis(300),
        None,
    )
    .await
    .unwrap_err();
    assert!(format!("{failed:#}").contains("delivered no frame"), "{failed:#}");
    let request = request.await.unwrap();
    assert!(request.contains("from=0"), "the stream resumed at the copy's last command: {request}");
    assert!(!request.contains("after="));
    server.abort();
}

#[tokio::test]
async fn follow_repeats_the_tail_without_reappending_it() {
    let operator = scratch_dir("standby-follow-repeat-operator");
    ticks(&operator, 6).await;
    let dir = scratch_dir("standby-follow-repeat-copy");
    copy_log(&operator, &dir);
    // The operator carries on; the resumed stream repeats the copy's tail before the new entries.
    ticks(&operator, 4).await;
    let (mut writer, mut copied) = open_copy(&dir, 100, Chain::new(DOMAIN, enclave())).unwrap();
    let head = copied.head().unwrap();
    let frames = log_messages(&operator, Offset::start_of(head.seq))
        .into_iter()
        .map(|message| (By::Operator, message))
        .collect();
    let (port, server, _) = operator_stream(frames).await;
    follow(
        &mut writer,
        &mut copied,
        &config(),
        &Followed::from_standby(&standby_section(port)),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap();
    writer.flush().unwrap();
    drop(writer);
    // Every repeat was recognised, not appended again: the copy is the operator's log exactly.
    assert_eq!(log_entries(&dir), log_entries(&operator));
    crate::sequencer::replay_path(&dir).unwrap();
    server.abort();
}

#[tokio::test]
async fn follow_stops_at_an_operator_cut_back_and_keeps_its_copy() {
    let operator = scratch_dir("standby-cut-back-operator");
    ticks(&operator, 8).await;
    let dir = scratch_dir("standby-cut-back-copy");
    copy_log(&operator, &dir);
    let before = log_entries(&dir);
    let (mut writer, mut copied) = open_copy(&dir, 100, Chain::new(DOMAIN, enclave())).unwrap();
    let head = copied.head().unwrap();
    // The host cuts the operator's log back and restarts it, and it sequences other commands past
    // the copy's head.
    cut_back(&operator, head.seq);
    ticks(&operator, 10).await;
    let frames = log_messages(&operator, Offset::start_of(head.seq)).into_iter().map(|message| (By::Operator, message));
    let (port, server, request) = operator_stream(frames.collect()).await;

    let err = follow(
        &mut writer,
        &mut copied,
        &config(),
        &Followed::from_standby(&standby_section(port)),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap_err();
    let rollback = rollback_of(&err).unwrap_or_else(|| panic!("a rollback: {err:#}"));
    assert_eq!(rollback.copy_head, Some(head));
    assert!(rollback.evidence.starts_with(&format!("its entry at {}:0 differs", head.seq)), "{}", rollback.evidence);
    assert!(request.await.unwrap().contains(&format!("from={}", head.seq)));
    drop(writer);
    // The copy is as it was, and still replays to its roots.
    assert_eq!(log_entries(&dir), before);
    crate::sequencer::replay_path(&dir).unwrap();
    server.abort();
}

#[tokio::test]
async fn follow_stops_at_a_signed_head_behind_its_copy() {
    let operator = scratch_dir("standby-head-behind-operator");
    ticks(&operator, 2).await;
    let dir = scratch_dir("standby-head-behind-copy");
    copy_log(&operator, &dir);
    let before = log_entries(&dir);
    let (mut writer, mut copied) = open_copy(&dir, 100, Chain::new(DOMAIN, enclave())).unwrap();
    let head = copied.head().unwrap();
    let behind = Offset::start_of(head.seq - 1);
    let heartbeat = serde_json::json!({ "type": "heartbeat", "head": behind.to_string() }).to_string();
    let (port, server, _) = operator_stream(vec![(By::Operator, heartbeat)]).await;

    let err = follow(
        &mut writer,
        &mut copied,
        &config(),
        &Followed::from_standby(&standby_section(port)),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap_err();
    let rollback = rollback_of(&err).unwrap_or_else(|| panic!("a rollback: {err:#}"));
    assert_eq!(
        rollback.evidence,
        format!("its signed head is {behind}, so it no longer holds the copy's entries after {behind} through {head}")
    );
    drop(writer);
    assert_eq!(log_entries(&dir), before);
    server.abort();
}

// ── Helpers ──

/// The deployment `test_support::sequencer_config` seals and signs for.
pub(super) const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
const DEPLOYMENT: Deployment = Deployment { chain_id: 31_337, vault: [0; 20] };

/// Who signs a frame the stand-in operator sends.
enum By {
    Operator,
    Other,
}

/// The test sequencer's enclave address, which seals its commands and signs its stream.
fn enclave() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}

/// The dev config on the test sequencer's deployment.
fn config() -> NodeConfig {
    let mut cfg = dev_config();
    cfg.custody.chain_id = DEPLOYMENT.chain_id;
    cfg.custody.vault = format!("0x{}", hex::encode(DEPLOYMENT.vault));
    cfg
}

/// A standby of the stand-in operator on `port`.
fn standby_section(port: u16) -> StandbySection {
    StandbySection {
        operator: format!("http://127.0.0.1:{port}"),
        mirrors: Vec::new(),
        enclave: enclave(),
        poll_interval: Duration::from_millis(50),
        danger_accept_invalid_certs: false,
    }
}

/// A stand-in operator's signed stream on a loopback port: answers one connection with `frames`,
/// each signed as the stream signs it, and reports the request it answered.
async fn operator_stream(
    frames: Vec<(By, String)>,
) -> (u16, tokio::task::JoinHandle<()>, tokio::sync::oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (request_tx, request_rx) = tokio::sync::oneshot::channel::<String>();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        // The client picks the challenge; the frames must bind what it sent, so the server reads
        // the request it answered.
        let (path_tx, path_rx) = tokio::sync::oneshot::channel::<String>();
        #[allow(clippy::result_large_err)]
        let callback =
            move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                  response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                let _ = path_tx.send(request.uri().path_and_query().map(|q| q.as_str().to_owned()).unwrap_or_default());
                Ok(response)
            };
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, callback).await.unwrap();
        let path = path_rx.await.unwrap();
        let challenge = path
            .split('&')
            .find_map(|part| part.strip_prefix("challenge="))
            .map(|hex| engine_wire::parse_challenge(hex).expect("a challenge the client sent"));
        let connection = Connection::new(challenge.unwrap(), &path);
        let _ = request_tx.send(path);
        let (operator, other) = (EnclaveKey::test_key(), EnclaveKey::generate());
        let mut previous = [0u8; 32];
        for (n, (by, message)) in frames.into_iter().enumerate() {
            let key = match by {
                By::Operator => &operator,
                By::Other => &other,
            };
            let frame = frame(key, &DEPLOYMENT, &connection, n as u64, &mut previous, message);
            // The standby closes the connection at the first frame it refuses.
            if ws.send(Message::text(frame)).await.is_err() {
                return;
            }
        }
        // Keep the connection open until the client closes it.
        let _ = futures_util::StreamExt::next(&mut ws).await;
    });
    (port, server, request_rx)
}

/// Runs the test sequencer on `dir`, from its log, for `count` ticks, then stops it as a shutdown
/// does.
pub(super) async fn ticks(dir: &std::path::Path, count: usize) {
    let node = TestSequencer::start(dir, 1024);
    for _ in 0..count {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
}

/// Cuts the log in `dir` back to its last checkpoint more than four commands before `seq`, as a
/// host that truncates it to an earlier accepted checkpoint does.
pub(super) fn cut_back(dir: &std::path::Path, seq: u64) {
    let earlier = log_entries(dir)
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) if checkpoint.seq + 4 < seq => Some(checkpoint),
            _ => None,
        })
        .expect("an earlier checkpoint");
    truncate_to_checkpoint(dir, earlier.seq, &earlier.root).unwrap();
}

/// Copies the log files in `from` into the empty directory `to`, as a standby's copy of them.
pub(super) fn copy_log(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for file in std::fs::read_dir(from).unwrap().map(Result::unwrap) {
        if file.file_type().unwrap().is_file() {
            std::fs::copy(file.path(), to.join(file.file_name())).unwrap();
        }
    }
}

#[test]
fn the_stint_alert_decision_keeps_the_two_alerts_apart() {
    let offset = Some(Offset { seq: 4, index: 0 });
    let stream_err = || anyhow::anyhow!("the stream broke");
    let rollback_err =
        || anyhow::Error::new(chain::Rollback { copy_head: offset, evidence: "a repeat differs".into() });
    use super::{StintAction, stint_action};
    // A stream failure is a follow failure even while a rollback episode is open.
    assert_eq!(stint_action(&Err(stream_err()), true, true), StintAction::FollowFailed);
    assert_eq!(stint_action(&Err(stream_err()), false, false), StintAction::FollowFailed);
    // A rollback is not a follow failure; the stream itself worked.
    assert!(matches!(stint_action(&Err(rollback_err()), true, true), StintAction::Rollback(_)));
    assert!(matches!(stint_action(&Err(rollback_err()), false, false), StintAction::Rollback(_)));
    // Only an Ok stint that repeated the whole tail resolves an open episode.
    assert_eq!(stint_action(&Ok(offset), true, true), StintAction::Resolved);
    assert_eq!(stint_action(&Ok(offset), true, false), StintAction::Quiet);
    assert_eq!(stint_action(&Ok(offset), false, true), StintAction::Quiet);
    assert_eq!(stint_action(&Ok(None), false, false), StintAction::Quiet);
}
