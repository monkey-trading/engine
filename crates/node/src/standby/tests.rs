//! The standby's copy: following verifies frames before appending entries, and the takeover
//! truncates the copy at the vault's latest checkpoint.

use std::time::Duration;

use engine_core::command::{Command, Sequenced};
use engine_wire::{Connection, Deployment, FRAME_VERSION, SignedFrame};
use futures_util::SinkExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::log::{Checkpoint, CommandEntry, LogEntry, Offset, SegmentedLog, truncate_to_checkpoint};
use crate::sequencer::StreamMsg;
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, scratch_dir};

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

fn msg(offset: Offset, entry: LogEntry) -> String {
    serde_json::to_string(&StreamMsg::new(offset, entry)).unwrap()
}

#[tokio::test]
async fn follow_appends_verified_frames_and_refuses_a_wrong_signer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let key = EnclaveKey::generate();
    let other = EnclaveKey::generate();
    let enclave = engine_core::types::AccountId(key.address());
    let deployment = Deployment { chain_id: 31_337, vault: [9; 20] };
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
        let mut previous = [0u8; 32];
        let messages = [
            msg(Offset { seq: 0, index: 0 }, command(0)),
            msg(Offset { seq: 0, index: 1 }, checkpoint(0, 5)),
            msg(Offset { seq: 1, index: 0 }, command(1)),
        ];
        for (n, message) in messages.into_iter().enumerate() {
            ws.send(Message::text(frame(&key, &deployment, &connection, n as u64, &mut previous, message)))
                .await
                .unwrap();
        }
        // One frame from another key: the standby must fail the connection, not append it.
        ws.send(Message::text(frame(
            &other,
            &deployment,
            &connection,
            3,
            &mut previous,
            msg(Offset { seq: 2, index: 0 }, command(2)),
        )))
        .await
        .unwrap();
        // Keep the connection open until the client closes it.
        let _ = futures_util::StreamExt::next(&mut ws).await;
    });

    let dir = scratch_dir("standby-follow");
    let mut cfg = dev_config();
    cfg.custody.chain_id = 31_337;
    cfg.custody.vault = format!("0x{}", hex::encode([9; 20]));
    let standby = StandbySection {
        operator: format!("http://127.0.0.1:{port}"),
        mirrors: Vec::new(),
        enclave,
        poll_interval: Duration::from_millis(50),
        danger_accept_invalid_certs: false,
    };
    let mut writer = open_writer(&dir, 100);
    let err = follow(&mut writer, &cfg, &standby, None, Duration::from_secs(5)).await.unwrap_err();
    assert!(format!("{err:#}").contains("failed verification"), "{err:#}");
    drop(writer);
    assert_eq!(entries(&dir), vec![command(0), checkpoint(0, 5), command(1)]);
    server.abort();
}

#[tokio::test]
async fn follow_resumes_after_the_last_offset() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let key = EnclaveKey::generate();
    let enclave = engine_core::types::AccountId(key.address());
    let (requests_tx, requests_rx) = std::sync::mpsc::channel::<String>();
    let server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let requests = requests_tx.clone();
            tokio::spawn(async move {
                #[allow(clippy::result_large_err)]
                let callback =
                    move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                          response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                        requests
                            .send(request.uri().path_and_query().map(|q| q.as_str().to_owned()).unwrap_or_default())
                            .unwrap();
                        Ok(response)
                    };
                let mut ws = match tokio_tungstenite::accept_hdr_async(stream, callback).await {
                    Ok(ws) => ws,
                    Err(_) => return,
                };
                // Nothing to send: the client only needs the connection to open.
                let _ = futures_util::StreamExt::next(&mut ws).await;
            });
        }
    });
    let dir = scratch_dir("standby-follow-resume");
    let mut writer = open_writer(&dir, 100);
    writer.append(&command(0)).unwrap();
    writer.append(&checkpoint(0, 5)).unwrap();
    writer.flush().unwrap();
    let mut cfg = dev_config();
    cfg.custody.chain_id = 31_337;
    cfg.custody.vault = format!("0x{}", hex::encode([9; 20]));
    let standby = StandbySection {
        operator: format!("http://127.0.0.1:{port}"),
        mirrors: Vec::new(),
        enclave,
        poll_interval: Duration::from_millis(50),
        danger_accept_invalid_certs: false,
    };
    let resume = Offset { seq: 0, index: 1 };
    let reached = follow(&mut writer, &cfg, &standby, Some(resume), Duration::from_millis(300)).await.unwrap();
    assert_eq!(reached, Some(resume));
    drop(writer);
    let request = requests_rx.recv().unwrap();
    assert!(request.contains("after=0:1"), "the stream resumed after the last offset: {request}");
    assert!(!request.contains("from="));
    server.abort();
}
