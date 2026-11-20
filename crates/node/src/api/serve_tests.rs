//! The API served on its own, for tests that need only its history or its signed stream: the
//! liveness scenario's followers connect to an operator's signed stream served this way.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Semaphore, broadcast, mpsc, watch};

use super::tests::{mock_attestation, serve_state};
use super::*;
use crate::clock::SystemClock;
use crate::config::RateLimit;
use crate::history::{INDEX_FILE, Reader};
use crate::log::Manifest;
use crate::test_support::{dev_config, scratch_dir};

/// Serves the API with only `history` behind it, so it outlives any sequencer. Its per-client rate
/// lets one client page a long log an event at a time.
pub(super) async fn serve_history(history: Reader) -> SocketAddr {
    serve_history_signing(history, crate::test_support::signing()).await
}

/// The API that serves only `signing`'s signed stream, empty of entries: what a follower connects to
/// for the heads and heartbeats `signing` carries. Its per-client rate is not limited.
pub(crate) async fn serve_signing(signing: Arc<crate::api::Signing>) -> SocketAddr {
    let dir = scratch_dir("serve-signing");
    serve_history_signing(Reader::new(dir.join(INDEX_FILE), watch::channel(true).1), signing).await
}

async fn serve_history_signing(history: Reader, signing: Arc<crate::api::Signing>) -> SocketAddr {
    let mut api = dev_config().api;
    let ample = NonZeroU32::new(1_000_000).unwrap();
    api.per_ip = RateLimit { per_second: ample, burst: ample };
    serve_state(ApiState {
        protocol: engine_core::protocol::ProtocolVersion::V23,
        access: None,
        tx: mpsc::channel(1).0,
        stream: broadcast::channel(1).0,
        attestation: mock_attestation(),
        auth: ClientAuth::Unsigned,
        operator: dev_config().operator().unwrap(),
        mock_custody: false,
        payouts_enabled: false,
        clock: Arc::new(SystemClock),
        log_dir: PathBuf::new(),
        daily_snapshots: None,
        manifest: watch::channel(Manifest::default()).1,
        backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
        limits: Arc::new(ApiLimits::new(&api)),
        history,
        marks: crate::test_support::marks_reader(),
        stats: crate::test_support::stats(),
        emission_budget: None,
        staking_terms: None,
        status: Default::default(),
        collateral: None,
        listing: Default::default(),
        operators: None,
        operator_hostname: None,
        signing,
    })
    .await
}
