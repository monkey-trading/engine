//! The daily snapshots the archive serves beside the log's segments.

use std::fs;

use engine_core::Engine;
use engine_core::command::{Command, Sequenced};

use crate::api::tests::{api_state, get, serve_state};
use crate::daily_snapshots::{Job, MANIFEST, publish_now};
use crate::log::{Checkpoint, Offset};
use crate::snapshot::sealing::Encoding;
use crate::test_support::scratch_dir;

/// The archive serves the daily snapshots' manifest and each snapshot it lists, as written, and answers
/// 404 for any other name, or when the node publishes none, and 500 for a snapshot that differs from its
/// listing.
#[tokio::test]
async fn the_archive_serves_the_daily_snapshots_it_lists_as_written() {
    let dir = scratch_dir("archive-daily");
    let (mut state, _requests) = api_state(true);
    let unpublished = serve_state(state.clone()).await;
    assert_eq!(get(unpublished, &format!("/v1/archive/snapshots/{MANIFEST}")).await.0, 404);
    state.daily_snapshots = Some(dir.clone());
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
    Job { offset: Offset { seq: 0, index: 1 }, checkpoint, ts: 0, engine }
}
