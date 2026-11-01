//! A watch-only transport failure cannot hide coalesced changes behind a publisher download.

use super::*;

use axum::extract::Request;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
enum Marker {
    Watch(Option<u64>),
    Held(u64),
    Failed,
    Admin(StatusCode),
}

#[derive(Clone)]
struct WatchFault {
    held: Arc<AtomicBool>,
    release: Arc<Notify>,
    markers: mpsc::Sender<Marker>,
}

#[tokio::test]
async fn benchmark_worker_reconnects_during_download_and_fences_missed_schedule_revisions() {
    let (markers, mut seen) = mpsc::channel(64);
    let release = Arc::new(Notify::new());
    let fault = WatchFault { held: Arc::new(AtomicBool::new(false)), release: release.clone(), markers };
    let mut scenario = Scenario::start_custom(
        "benchmark-watch-reconnect",
        EARLY,
        std::sync::Arc::new(crate::clock::test_clock::TestClock::at(EARLY)),
        "20s",
        move |app| app.layer(from_fn_with_state(fault, watch_fault)),
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        scenario.publisher.version.store(2, Ordering::SeqCst);
        scenario.publisher.blocked.store(2, Ordering::SeqCst);
        scenario.watch();
        scenario.next_request(2).await?;
        let Marker::Held(old_revision) =
            marker(&mut seen, Duration::from_secs(3), |event| matches!(event, Marker::Held(_))).await?
        else {
            unreachable!("marker predicate requires a held watch");
        };

        let original = dev_config().genesis.params;
        let mut moved = original.clone();
        moved.rollover.tz = chrono_tz::Asia::Tokyo;
        ensure!(signed_params(&scenario.node, moved).await?.contains(&Event::AdminApplied), "A to B applies");
        ensure!(signed_params(&scenario.node, original).await?.contains(&Event::AdminApplied), "B to A applies");
        let revision = durable_revision(&scenario.node).await?;
        ensure!(revision > old_revision, "an equal final schedule still has a newer durable revision");
        ensure!(balance(&scenario.node).await? == fixed("100000"), "unpaid dates cannot charge during the gap");
        scenario.publisher.version.store(3, Ordering::SeqCst);
        release.notify_one();
        marker(&mut seen, Duration::from_secs(3), |event| matches!(event, Marker::Failed)).await?;

        // The ten-second watch backoff must finish while the twenty-second publisher stays pending.
        marker(&mut seen, Duration::from_secs(12), |event| matches!(event, Marker::Watch(None))).await?;
        marker(
            &mut seen,
            Duration::from_secs(3),
            |event| matches!(event, Marker::Watch(Some(after)) if *after == revision),
        )
        .await?;
        ensure!(
            !scenario.worker.as_ref().context("running worker")?.is_finished(),
            "watch failure must not cancel the owning worker"
        );
        ensure!(
            scenario.publisher.requests.try_recv().is_err(),
            "reconnection does not abandon the original download or start concurrent snapshots"
        );

        scenario.publisher.release.notify_one();
        marker(
            &mut seen,
            Duration::from_secs(3),
            |event| matches!(event, Marker::Admin(status) if *status == StatusCode::CONFLICT),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(3), scenario.next_request(3))
            .await
            .context("coalesced reconnect update triggers an immediate fresh attempt, ahead of ten-second retry")??;
        marker(
            &mut seen,
            Duration::from_secs(3),
            |event| matches!(event, Marker::Admin(status) if *status == StatusCode::OK),
        )
        .await?;
        wait_nonce(&scenario.node, 4).await?;
        scenario.node.advance(FIRST - 30_000).await;
        scenario.node.advance(FIRST + 1).await;
        ensure!(
            charges(&scenario.node.submit(Command::Tick).await) == [charge("0.03")],
            "only the fresh post-reconnect signed observation finances the date"
        );
        ensure!(
            balance(&scenario.node).await? == fixed("100000") - charge("0.03"),
            "the stale request and watch gap change no financing balance"
        );
        anyhow::Ok(())
    })
    .await;
    release.notify_waiters();
    let dir = scenario.finish().await;
    outcome.expect("watch reconnect scenario finishes within its bounded deadline").expect("reconnect invariant");
    assert_logged_financing(&dir, "0.03", Some(2));
    assert!(
        log_entries(&dir).iter().all(|entry| {
            let LogEntry::Command(command) = entry else { return true };
            let Command::SignedAdmin(request) = &command.sequenced.cmd else { return true };
            let AdminCommand::SetBenchmarkRates(batch) = &request.command else { return true };
            batch.rates.iter().all(|rate| rate.benchmark != "SOFR" || rate.annual_rate != fixed("0.02"))
        }),
        "the actual HTTP409 fences the stale observation before it enters the public log"
    );
}

/// Holds one long-poll before subscription, then returns a real HTTP503 only for that watch.
async fn watch_fault(State(fault): State<WatchFault>, request: Request, next: Next) -> Response {
    let watch = request.uri().path() == "/v1/benchmarks/schedule";
    let admin = request.method() == axum::http::Method::POST && request.uri().path() == "/v1/admin";
    if watch {
        let after = request
            .uri()
            .query()
            .and_then(|query| query.strip_prefix("after="))
            .map(|value| value.parse::<u64>().expect("the production watch sends an unsigned revision"));
        fault.markers.try_send(Marker::Watch(after)).expect("bounded watch marker capacity");
        if let Some(revision) = after
            && !fault.held.swap(true, Ordering::SeqCst)
        {
            fault.markers.try_send(Marker::Held(revision)).expect("bounded watch marker capacity");
            fault.release.notified().await;
            fault.markers.try_send(Marker::Failed).expect("bounded watch marker capacity");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    let response = next.run(request).await;
    if admin {
        fault.markers.try_send(Marker::Admin(response.status())).expect("bounded admin marker capacity");
    }
    response
}

/// Waits for a transport observation without using a sleep to guess progress.
/// Error cases:
/// - the observation channel closes or the deadline expires
async fn marker(
    markers: &mut mpsc::Receiver<Marker>,
    within: Duration,
    matches: impl Fn(&Marker) -> bool,
) -> anyhow::Result<Marker> {
    tokio::time::timeout(within, async {
        while let Some(marker) = markers.recv().await {
            if matches(&marker) {
                return Ok(marker);
            }
        }
        anyhow::bail!("watch observation channel closed")
    })
    .await
    .context("expected watch/admin transport observation before deadline")?
}

/// Reads the post-batch revision after both accepted schedule commands have been acknowledged.
/// Error cases:
/// - the sequencer cannot serve its consistent already-flushed query
async fn durable_revision(node: &Node) -> anyhow::Result<u64> {
    let (reply, response) = oneshot::channel();
    node.tx
        .send(SeqRequest::Query(Box::new(move |engine| {
            let _ = reply.send(engine.last_seq().map_or(0, |seq| seq + 1));
        })))
        .await
        .map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
    response.await.context("durable schedule revision")
}
