//! The predecessor's published log ahead of a handover: an archive that is unreachable or answers an
//! error leaves the start to the handover, which carries the log, while one whose listings fail their
//! checks refuses it, with or without a local copy from an earlier start.

use super::*;

use std::collections::BTreeSet;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::net::TcpListener;

use crate::standby::archive::ArchiveIntegrity;
use crate::test_support::scratch_dir;

/// What the stand-in archive answers for its manifest.
#[derive(Clone)]
enum Answer {
    /// `503 Service Unavailable`.
    Unavailable,
    /// The manifest, one listing a line.
    Manifest(String),
}

#[tokio::test]
async fn an_archive_that_is_unreachable_or_answers_an_error_leaves_the_start_to_the_handover() {
    let unreachable = format!("http://{}", closed_port());
    let unavailable = serve(Answer::Unavailable).await;
    for (what, mirror) in [("unreachable", &unreachable), ("answering 503", &unavailable)] {
        for local in [false, true] {
            let cfg = mirrored(&format!("archive-transport-{}-{local}", what.replace(' ', "-")), mirror, local);
            let synced = sync_predecessor_log(&cfg, &incumbent(), BTreeSet::new()).await;
            synced.unwrap_or_else(|e| panic!("an archive {what} (local copy: {local}) refused the start: {e:#}"));
        }
    }
}

#[tokio::test]
async fn an_archive_whose_listings_fail_their_checks_refuses_the_start() {
    let first = listing("00000000000000000000.jsonl", 0);
    for (case, (what, manifest)) in [
        ("a listing named for another seq", listing("00000000000000000007.jsonl", 0)),
        ("listings out of order", format!("{}{first}", listing("00000000000000000005.jsonl", 5))),
    ]
    .into_iter()
    .enumerate()
    {
        let mirror = serve(Answer::Manifest(manifest)).await;
        for local in [false, true] {
            let cfg = mirrored(&format!("archive-integrity-{case}-{local}"), &mirror, local);
            let err = sync_predecessor_log(&cfg, &incumbent(), BTreeSet::new())
                .await
                .err()
                .unwrap_or_else(|| panic!("{what} (local copy: {local}) passed"));
            assert!(err.downcast_ref::<ArchiveIntegrity>().is_some(), "{what}: not an integrity failure: {err:#}");
            assert!(format!("{err:#}").contains("failed integrity checks"), "{what}: {err:#}");
        }
    }
}

// ── Helpers ──

/// A config whose only archive mirror is `mirror`, with a fresh log directory that holds an empty
/// local copy when `local`, as an earlier start leaves one.
fn mirrored(name: &str, mirror: &str, local: bool) -> NodeConfig {
    let mut cfg = crate::test_support::dev_config();
    let log_dir = scratch_dir(name);
    if local {
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(log_dir.join(crate::log::MANIFEST), "").unwrap();
    }
    cfg.node.log_dir = log_dir;
    let sealing = format!(
        "dir = {:?}\n[handover]\nlisten = '127.0.0.1:0'\nmirrors = [{mirror:?}]",
        scratch_dir(&format!("{name}-keys")).display().to_string()
    );
    cfg.tee.sealing = Some(toml::from_str(&sealing).unwrap());
    cfg
}

/// The incumbent, whose own API hostname nothing answers on.
fn incumbent() -> Source {
    Source {
        endpoint: closed_port(),
        api_hostname: closed_port(),
        release: [0x22; 32],
        candidate: AccountId([0x42; 20]),
    }
}

/// A loopback address nothing listens on.
fn closed_port() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

/// One manifest line listing `file` from `first_seq`, with the hash and size of no bytes served.
fn listing(file: &str, first_seq: u64) -> String {
    let listed = serde_json::json!({
        "file": file,
        "first_seq": first_seq,
        "last_seq": first_seq + 1,
        "bytes": 10,
        "sha256": "00".repeat(32),
    });
    format!("{listed}\n")
}

/// Serves `answer` as the archive's manifest on loopback, and no segment: its base URL.
async fn serve(answer: Answer) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/v1/archive/{file}",
        get(move |axum::extract::Path(file): axum::extract::Path<String>| {
            let answer = answer.clone();
            async move {
                match (file.as_str(), answer) {
                    (crate::log::MANIFEST, Answer::Manifest(manifest)) => manifest.into_response(),
                    (_, Answer::Unavailable) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}
