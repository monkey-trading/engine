//! Schedule long-polls release capped places on response, cancellation, and sequencer shutdown.

use super::*;

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engine_core::auth::ClientAuth;
use futures_util::poll;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::time::timeout;

use crate::api::MAX_BACKFILLS;
use crate::clock::SystemClock;
use crate::history::Reader;
use crate::limits::ApiLimits;
use crate::log::Manifest;
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{dev_config, scratch_dir};

#[tokio::test]
async fn schedule_initial_and_mismatched_revisions_return_the_current_durable_view() {
    for after in [None, Some(2), Some(9)] {
        let (state, mut requests) = state();
        let limits = state.limits.clone();
        let mut response = Box::pin(schedule(State(state), Query(ScheduleParams { after })));
        assert!(poll!(&mut response).is_pending());
        let Request::WatchBenchmarkSchedule(reply) = requests.recv().await.unwrap() else {
            panic!("schedule reads request the sequencer's durable subscription");
        };
        let (updates, receiver) = watch::channel(view(3));
        assert!(reply.send(receiver).is_ok());
        let result = timeout(Duration::from_secs(1), response).await.unwrap().unwrap().0;
        assert_eq!(result, view(3));
        assert!(limits.subscribe().is_ok(), "responding releases the shared subscriber slot");
        drop(updates);
    }
}

#[tokio::test]
async fn schedule_waits_for_the_next_revision_and_coalesces_changes_without_losing_a_to_b_to_a() {
    let (state, mut requests) = state();
    let mut response = Box::pin(schedule(State(state), Query(ScheduleParams { after: Some(3) })));
    assert!(poll!(&mut response).is_pending());
    let Request::WatchBenchmarkSchedule(reply) = requests.recv().await.unwrap() else { panic!("schedule watch") };
    let (updates, receiver) = watch::channel(view(3));
    assert!(reply.send(receiver).is_ok());
    assert!(poll!(&mut response).is_pending(), "the known revision does not busy-poll");
    let mut changed = view(4);
    changed.rollover.as_mut().unwrap().minute = 1;
    updates.send_replace(changed);
    updates.send_replace(view(5));
    let result = timeout(Duration::from_secs(1), response).await.unwrap().unwrap().0;
    assert_eq!(result, view(5), "latest revision matters even if the schedule returned to its old value");
}

#[tokio::test]
async fn schedule_wait_timeout_returns_the_current_view_inside_the_transport_budget() {
    let (state, mut requests) = state();
    let limits = state.limits.clone();
    let started = Instant::now();
    let mut response = Box::pin(schedule(State(state), Query(ScheduleParams { after: Some(3) })));
    assert!(poll!(&mut response).is_pending());
    let Request::WatchBenchmarkSchedule(reply) = requests.recv().await.unwrap() else { panic!("schedule watch") };
    let (_updates, receiver) = watch::channel(view(3));
    assert!(reply.send(receiver).is_ok());
    assert!(poll!(&mut response).is_pending());
    let result = timeout(CALL_TIMEOUT, response).await.unwrap().unwrap().0;
    assert_eq!(result, view(3));
    assert!(started.elapsed() >= CALL_TIMEOUT / 2, "unchanged watches wait for their actual bounded timer");
    assert!(limits.subscribe().is_ok());
}

#[tokio::test]
async fn schedule_shares_subscriber_caps_and_cancellation_releases_its_place() {
    let (state, mut requests) = state();
    let limits = state.limits.clone();
    let other = state.clone();
    let mut response = Box::pin(schedule(State(state), Query(ScheduleParams { after: Some(3) })));
    assert!(poll!(&mut response).is_pending());
    let Request::WatchBenchmarkSchedule(reply) = requests.recv().await.unwrap() else { panic!("schedule watch") };
    let (_updates, receiver) = watch::channel(view(3));
    assert!(reply.send(receiver).is_ok());
    assert!(poll!(&mut response).is_pending());
    assert!(matches!(
        schedule(State(other), Query(ScheduleParams { after: None })).await,
        Err(ApiError::TooManySubscribers)
    ));
    assert!(requests.try_recv().is_err(), "over-cap request never reaches the sequencer");
    drop(response);
    assert!(limits.subscribe().is_ok(), "cancellation returns the slot");
}

#[tokio::test]
async fn schedule_sequencer_and_watch_shutdown_return_unavailable_and_release_the_slot() {
    for close_watch in [false, true] {
        let (state, mut requests) = state();
        let limits = state.limits.clone();
        let mut response = Box::pin(schedule(State(state), Query(ScheduleParams { after: Some(3) })));
        assert!(poll!(&mut response).is_pending());
        let Request::WatchBenchmarkSchedule(reply) = requests.recv().await.unwrap() else { panic!("schedule watch") };
        if close_watch {
            let (updates, receiver) = watch::channel(view(3));
            assert!(reply.send(receiver).is_ok());
            drop(updates);
        } else {
            drop(reply);
        }
        assert!(matches!(timeout(Duration::from_secs(1), response).await.unwrap(), Err(ApiError::Unavailable)));
        assert!(limits.subscribe().is_ok());
    }
}

fn view(revision: u64) -> BenchmarkSchedule {
    BenchmarkSchedule { revision, rollover: Some(dev_config().genesis.params.rollover) }
}

fn state() -> (ApiState, mpsc::Receiver<Request>) {
    let (tx, requests) = mpsc::channel(1);
    let mut cfg = dev_config();
    cfg.api.max_subscribers = NonZeroUsize::new(1).unwrap();
    let bundle = AttestationBundle {
        enclave_address: EnclaveKey::test_key().address(),
        config_sha256: [0; 32],
        tls_public_key_sha256: [0; 32],
    };
    let state = ApiState {
        protocol: engine_core::protocol::ProtocolVersion::V23,
        access: None,
        tx,
        stream: tokio::sync::broadcast::channel(1).0,
        attestation: Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap()),
        auth: ClientAuth::Unsigned,
        operator: cfg.operator().unwrap(),
        mock_custody: true,
        payouts_enabled: true,
        clock: Arc::new(SystemClock),
        log_dir: scratch_dir("benchmark-schedule-api"),
        manifest: watch::channel(Manifest::default()).1,
        backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
        limits: Arc::new(ApiLimits::new(&cfg.api)),
        history: Reader::new(PathBuf::new(), watch::channel(true).1),
        marks: crate::test_support::marks_reader(),
        emission_budget: None,
        collateral: None,
        listing: Default::default(),
        signing: crate::test_support::signing(),
    };
    (state, requests)
}
