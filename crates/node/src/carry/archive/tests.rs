use std::path::Path;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use engine_core::types::AccountId;
use tokio::net::TcpListener;

use super::*;
use crate::log::SegmentedLog;
use crate::test_support::{TestSequencer, dev_config, log_entries, scratch_dir};

/// The node's reference log whose genesis carries the reference session's state.
const CARRIED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v90-carried.jsonl");

/// The entries of the log file at `path`.
fn entries(path: &Path) -> Vec<crate::log::LogEntry> {
    let mut entries = Vec::new();
    crate::log::read_file(path, |entry| {
        entries.push(entry);
        Ok(())
    })
    .unwrap();
    entries
}

/// `entries` as a segmented log in `dir` of three commands a segment, so its genesis spans several.
fn segmented(entries: &[crate::log::LogEntry], dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let log = SegmentedLog::open(dir, |_| Ok(())).unwrap();
    let mut writer = log.into_writer(3, tokio::sync::watch::channel(Default::default()).0).unwrap();
    for entry in entries {
        writer.append(entry).unwrap();
    }
    writer.flush().unwrap();
}

/// Serves the files in `dir` as an archive mirror, over plain HTTP on loopback: the operator it archives for.
async fn archived(dir: &Path) -> Followed {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let dir = dir.to_owned();
    let app = Router::new().route(
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
    Followed { origin, enclave: AccountId([1; 20]), mirrors: Vec::new(), danger_accept_invalid_certs: true }
}

/// The archived genesis is read across its segments: the carried log's source, a dev log's nothing, and, while the
/// archive holds no whole genesis, no answer; a config is held to it as the start would hold it.
#[tokio::test]
async fn a_member_reads_the_archived_genesis_and_is_held_to_its_carry() {
    let carried_dir = scratch_dir("carry-archive-carried");
    segmented(&entries(Path::new(CARRIED)), &carried_dir);
    let carried = archived(&carried_dir).await;
    let client = archive_client(true).unwrap();
    let source = archived_source(&client, &carried.origins()).await.unwrap().flatten().expect("a carried genesis");

    let written = scratch_dir("carry-archive-written");
    let node = TestSequencer::start(&written, 16);
    for _ in 0..6 {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
    let dev_dir = scratch_dir("carry-archive-dev");
    segmented(&log_entries(&written), &dev_dir);
    let dev = archived(&dev_dir).await;
    assert_eq!(archived_source(&client, &dev.origins()).await.unwrap(), Some(None));

    let early_dir = scratch_dir("carry-archive-early");
    segmented(&entries(Path::new(CARRIED))[..8], &early_dir);
    let early = archived(&early_dir).await;
    assert_eq!(archived_source(&client, &early.origins()).await.unwrap(), None, "no whole genesis yet");

    let config = |carry: Option<AccountId>| {
        let mut cfg = dev_config();
        cfg.custody.chain_id = source.chain_id;
        cfg.genesis.carry = carry.map(|vault| crate::config::CarrySection { vault, file: "carry.json".into() });
        cfg
    };
    let (named, none) = (config(Some(source.vault)), config(None));
    assert!(check_archived(&named, &carried).await.is_ok());
    assert!(check_archived(&none, &carried).await.is_err(), "a carried log under a config naming none");
    assert!(check_archived(&named, &dev).await.is_err(), "a log that carried nothing under a config naming a carry");
    assert!(check_archived(&none, &dev).await.is_ok());
    assert!(check_archived(&named, &early).await.is_ok(), "an archive before its genesis");
    let down = Followed { origin: "http://127.0.0.1:9".into(), ..early };
    assert!(check_archived(&none, &down).await.is_err(), "an archive that cannot be read");
}
