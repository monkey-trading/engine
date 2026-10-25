//! Archive responses over the lazy catalog: canonical bytes and rechecked segments.

use super::*;

use axum::body::to_bytes;
use axum::extract::{Path as FilePath, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use futures_util::FutureExt;

use crate::log::authenticated_tests::{command, fixture};

#[tokio::test]
async fn lazy_archive_keeps_canonical_manifest_order_bytes_type_and_segment_lengths() {
    let mut f = fixture("lazy-archive-canonical", 2);
    for seq in 0..9 {
        f.writer.append(&command(seq)).expect("append");
    }
    f.writer.sync().expect("sync");
    assert!(f.view.borrow().segments.is_empty());
    let expected_manifest = fs::read(f.dir.join(MANIFEST)).expect("canonical writer bytes");
    let (mut state, _rx) = api_state(true);
    state.log_dir = f.dir.clone();
    state.manifest = f.view.clone();
    let forged = [expected_manifest.as_slice(), b"host forged suffix\n"].concat();
    fs::write(f.dir.join(MANIFEST), forged).expect("host changes locator");
    let response = crate::api::archive::archive(State(state.clone()), FilePath(MANIFEST.to_owned()))
        .await
        .expect("manifest response");
    assert_eq!(response.headers()[CONTENT_TYPE], "application/x-ndjson");
    assert_eq!(to_bytes(response.into_body(), usize::MAX).await.expect("manifest body"), expected_manifest);
    let view = f.view.borrow().clone();
    for ordinal in 0..view.completed_len() {
        let info = view.completed_at(ordinal).expect("verified descriptor");
        let expected = fs::read(f.dir.join(&info.file)).expect("original segment");
        let response = crate::api::archive::archive(State(state.clone()), FilePath(info.file.clone()))
            .await
            .expect("segment response");
        assert_eq!(response.headers()[CONTENT_TYPE], "application/x-ndjson");
        assert_eq!(response.headers()[CONTENT_LENGTH].to_str().expect("length header"), info.bytes.to_string());
        assert_eq!(to_bytes(response.into_body(), usize::MAX).await.expect("segment body"), expected);
    }
    let active = f.view.borrow().active.as_ref().expect("active segment").info.file.clone();
    assert!(matches!(
        crate::api::archive::archive(State(state.clone()), FilePath(active)).await,
        Err(ApiError::NotFound)
    ));
    for name in ["../manifest.jsonl", "nothing.jsonl", "00000000000000000003.jsonl"] {
        assert!(
            matches!(
                crate::api::archive::archive(State(state.clone()), FilePath(name.to_owned())).await,
                Err(ApiError::NotFound)
            ),
            "{name}"
        );
    }
}

#[tokio::test]
async fn lazy_archive_rejects_a_segment_changed_before_open() {
    let mut f = fixture("lazy-archive-corrupt-open", 2);
    for seq in 0..5 {
        f.writer.append(&command(seq)).expect("append");
    }
    f.writer.flush().expect("flush");
    let info = f.view.borrow().completed_at(0).expect("completed descriptor");
    let path = f.dir.join(&info.file);
    let text = fs::read_to_string(&path).expect("segment bytes");
    let changed = text.replacen("\"ts\":1,", "\"ts\":9,", 1);
    assert_ne!(text, changed);
    fs::write(path, changed).expect("host tamper");
    let (mut state, _rx) = api_state(true);
    state.log_dir = f.dir;
    state.manifest = f.view;
    assert!(matches!(crate::api::archive::archive(State(state), FilePath(info.file)).await, Err(ApiError::Internal)));
}

#[tokio::test]
async fn archive_body_rechecks_a_later_chunk_changed_after_response_open() {
    let mut f = fixture("lazy-archive-toctou", 20);
    for seq in 0..21 {
        f.writer.append(&command(seq)).expect("append");
    }
    f.writer.flush().expect("flush");
    let info = f.view.borrow().completed_at(0).expect("completed descriptor");
    let (mut state, _rx) = api_state(true);
    state.log_dir = f.dir.clone();
    state.manifest = f.view;
    let response = crate::api::archive::archive(State(state), FilePath(info.file.clone()))
        .await
        .expect("response opens before host rewrite");
    let path = f.dir.join(info.file);
    let text = fs::read_to_string(&path).expect("segment bytes");
    let changed = text.replacen("\"ts\":12,", "\"ts\":99,", 1);
    assert_ne!(text, changed);
    fs::write(path, changed).expect("host rewrites a later chunk");
    assert!(to_bytes(response.into_body(), usize::MAX).await.is_err(), "no successful body containing forged bytes");
}

#[tokio::test]
async fn an_unsigned_host_manifest_is_never_served_as_an_authoritative_archive() {
    let mut f = fixture("lazy-archive-host-authority", 1);
    for seq in 0..3 {
        f.writer.append(&command(seq)).expect("append");
    }
    f.writer.flush().expect("flush");
    let host = Manifest::load(&f.dir).expect("host locators");
    let (mut state, _rx) = api_state(true);
    state.log_dir = f.dir;
    state.manifest = watch::channel(host).1;
    assert!(matches!(
        crate::api::archive::archive(State(state), FilePath(MANIFEST.to_owned())).await,
        Err(ApiError::Internal)
    ));
}

/// A segment download that has opened but whose body the client has not read holds no backfill
/// permit, so slow archive clients cannot starve stream backfill.
#[tokio::test]
async fn an_open_segment_download_holds_no_backfill_permit() {
    let mut f = fixture("lazy-archive-permit", 2);
    for seq in 0..5 {
        f.writer.append(&command(seq)).expect("append");
    }
    f.writer.sync().expect("sync");
    let (mut state, _rx) = api_state(true);
    state.log_dir = f.dir.clone();
    state.manifest = f.view.clone();
    let info = f.view.borrow().completed_at(0).expect("verified descriptor");
    let response = crate::api::archive::archive(State(state.clone()), FilePath(info.file.clone()))
        .await
        .expect("segment response");
    assert_eq!(state.backfills.available_permits(), MAX_BACKFILLS, "the unread body holds no permit");
    let expected = fs::read(f.dir.join(&info.file)).expect("original segment");
    assert_eq!(to_bytes(response.into_body(), usize::MAX).await.expect("segment body"), expected);
}

/// A manifest request dropped mid-walk, as hyper drops one whose client disconnects, still caches
/// the listing its walk proves, and the next request reuses it instead of walking again.
#[test]
fn a_dropped_manifest_request_still_caches_its_listing() {
    // The only blocking thread is held busy, so the request's walk is still queued when it drops.
    let runtime =
        tokio::runtime::Builder::new_current_thread().max_blocking_threads(1).enable_all().build().expect("runtime");
    runtime.block_on(async {
        let mut f = fixture("lazy-archive-dropped-manifest", 2);
        for seq in 0..9 {
            f.writer.append(&command(seq)).expect("append");
        }
        f.writer.sync().expect("sync");
        let expected = fs::read(f.dir.join(MANIFEST)).expect("canonical writer bytes");
        let (mut state, _rx) = api_state(true);
        state.log_dir = f.dir.clone();
        state.manifest = f.view.clone();
        let (started, busy) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let occupier = tokio::task::spawn_blocking(move || {
            started.send(()).expect("the test waits for the busy thread");
            gate.recv()
        });
        busy.await.expect("the blocking thread is busy");
        let request = crate::api::archive::archive(State(state.clone()), FilePath(MANIFEST.to_owned()));
        // Polls the request once, so it starts its walk, then drops it.
        let waiting = request.now_or_never().is_none();
        release.send(()).expect("the busy thread waits for release");
        assert!(waiting, "the request waits on its queued walk");
        occupier.await.expect("busy thread").expect("released");
        let cached = state.listing.lock().await.clone().expect("the dropped request's walk is cached");
        assert_eq!(*cached.bytes, *expected);
        let response = crate::api::archive::archive(State(state.clone()), FilePath(MANIFEST.to_owned()))
            .await
            .expect("manifest response");
        assert_eq!(to_bytes(response.into_body(), usize::MAX).await.expect("manifest body"), expected);
        let reused = state.listing.lock().await.clone().expect("cached listing");
        assert!(Arc::ptr_eq(&reused.bytes, &cached.bytes), "the next request reuses the cached listing");
    });
}
