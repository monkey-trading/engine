//! A batch leaves the sequencer only once its sync is done, the next one is sequenced meanwhile, and
//! a failed sync stops the node. The sync is a gate the test opens, so no test sleeps to order
//! events. What the API answers a client waits for the same gate: a refusal states the state the
//! speculative engine had taken in, and the batch's `later` answers go out with its release.

use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender, channel};
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::Request as HttpRequest;
use axum::http::StatusCode;
use engine_core::auth::{ClientAuth, RequestAuth};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::eip712::{self, order_hash};
use engine_core::event::Event;
use engine_core::types::{AccountId, Side};
use k256::ecdsa::SigningKey;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tower::ServiceExt;

use super::*;
use crate::log::Manifest;
use crate::sequencer::Reissue;
use crate::test_support::{dev_config, sample_around, scratch_dir, sequencer_config};

/// What a sync does when the test lets it.
enum Verdict {
    Pass,
    Fail,
}

/// A running sequencer whose syncs wait for the test.
struct Gated {
    tx: mpsc::Sender<Request>,
    permits: StdSender<Verdict>,
    stream: broadcast::Receiver<Arc<StreamMsg>>,
    stats: crate::stats::Stats,
    /// Syncs the thread has started.
    syncs: Arc<AtomicUsize>,
    /// One message each time the thread starts a sync, before it waits for its verdict.
    entered: StdReceiver<()>,
    /// The log writer's manifest, as the API watches it.
    manifest: watch::Receiver<Manifest>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Gated {
    fn start(name: &str) -> Self {
        let dir = scratch_dir(name);
        let (stream, stream_rx) = broadcast::channel(256);
        let (withdrawals, _custody) = mpsc::unbounded_channel();
        let clock = Arc::new(crate::clock::test_clock::TestClock::default());
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let cfg = crate::sequencer::SequencerConfig { clock: clock.clone(), ..sequencer_config(&dir, manifest_tx) };
        let mut seq =
            Sequencer::recover(cfg, Arc::new(crate::tee::EnclaveKey::test_key()), stream, withdrawals).unwrap();
        let stats = crate::test_support::stats();
        seq.track_stats(stats.clone());
        let (permits, verdicts): (StdSender<Verdict>, StdReceiver<Verdict>) = channel();
        let verdicts = Mutex::new(verdicts);
        let syncs = Arc::new(AtomicUsize::new(0));
        let counted = syncs.clone();
        let (entering, entered) = channel();
        seq.pipeline_with_syncs(move |job| {
            _ = entering.send(());
            let verdict = verdicts.lock().unwrap().recv();
            match verdict {
                Ok(Verdict::Pass) => {
                    counted.fetch_add(1, Ordering::SeqCst);
                    job.sync()
                }
                _ => anyhow::bail!("the disk failed"),
            }
        })
        .unwrap();
        let weighting = dev_config().genesis.params.toxicity.notional.expect("the dev config weighs by notional");
        seq.carry_toxicity_notional(weighting).unwrap();
        seq.activate_protocol().unwrap();
        seq.genesis(&dev_config().genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(seq.run(rx));
        Self { tx, permits, stream: stream_rx, stats, syncs, entered, manifest, task }
    }

    /// Waits until the sync thread has started a sync and is waiting for its verdict.
    fn sync_started(&self) {
        self.entered.recv_timeout(Duration::from_secs(10)).expect("the sync thread starts a sync");
    }

    /// Submits a tick and returns its reply.
    async fn tick(&self) -> oneshot::Receiver<Vec<Event>> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.unwrap();
        rx
    }

    /// The last seq the sequencer has applied, durable or not.
    async fn applied(&self) -> Option<u64> {
        let (reply, rx) = oneshot::channel();
        let query: crate::sequencer::Query = Box::new(move |engine| {
            let _ = reply.send(engine.last_seq());
        });
        self.tx.send(Request::Query(query)).await.unwrap();
        rx.await.unwrap()
    }

    /// The last seq the stats worker's copy has applied, as a view reads it.
    async fn viewed(&self) -> Option<u64> {
        let (reply, rx) = oneshot::channel();
        let reader = self.stats.reader().expect("the worker runs");
        assert!(reader.read(Box::new(move |engine| _ = reply.send(engine.last_seq()))).is_ok());
        timeout(Duration::from_secs(10), rx).await.expect("the worker answers").unwrap()
    }
}

/// Whether `rx` has nothing yet, after the sequencer has had every chance to answer.
async fn silent<T>(rx: &mut oneshot::Receiver<T>) -> bool {
    timeout(Duration::from_millis(150), rx).await.is_err()
}

/// The node's API client surface over `node`: the same request channel and log manifest, with
/// signatures enforced under the chain and vault the sequencer config records.
fn api_over(node: &Gated) -> crate::api::ApiState {
    let (mut api, _unread) = crate::api::tests::api_state(true);
    api.tx = node.tx.clone();
    api.manifest = node.manifest.clone();
    api.auth = ClientAuth::Eip712 { chain_id: 31337, vault: AccountId([0; 20]) };
    api
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_is_released_only_after_its_sync_and_the_next_is_sequenced_meanwhile() {
    let node = Gated::start("pipeline-gated");
    // The genesis commands reached storage in line, before the loop ran.
    let before = node.applied().await;
    let mut first = node.tick().await;
    let applied = node.applied().await;
    assert!(applied > before, "the first tick is sequenced");
    assert!(silent(&mut first).await, "its reply waits for the sync");
    assert_eq!(node.viewed().await, before, "a view does not show it either");
    let mut stream = node.stream.resubscribe();

    // A second tick is sequenced while the first batch's sync is held.
    let mut second = node.tick().await;
    assert!(node.applied().await > applied, "the sequencer does not wait for the sync");
    assert!(silent(&mut second).await);
    assert!(stream.try_recv().is_err(), "nothing of either batch is on the stream");

    node.permits.send(Verdict::Pass).unwrap();
    first.await.expect("the first batch is released by its own sync");
    assert_eq!(node.viewed().await, applied, "a view sees the durable batch and not the next");
    assert!(silent(&mut second).await, "the second batch still waits for its own sync");
    node.permits.send(Verdict::Pass).unwrap();
    second.await.expect("the second batch is released in turn");
    let seqs: Vec<u64> = std::iter::from_fn(|| stream.try_recv().ok()).map(|msg| msg.offset.seq).collect();
    assert!(seqs.windows(2).all(|pair| pair[0] <= pair[1]), "the stream keeps log order: {seqs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_sync_releases_nothing_and_stops_the_sequencer() {
    let node = Gated::start("pipeline-failed");
    let reply = node.tick().await;
    node.permits.send(Verdict::Fail).unwrap();
    let stopped = timeout(Duration::from_secs(10), node.task).await.expect("the sequencer stops").unwrap();
    let error = format!("{:#}", stopped.expect_err("a failed sync is an error"));
    assert!(error.contains("syncing the log"), "{error}");
    assert!(reply.await.is_err(), "the reply of the undurable batch is dropped, not sent");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_pipeline_takes_no_request_until_a_sync_is_done() {
    let dir = scratch_dir("pipeline-bounded");
    let (stream, _) = broadcast::channel(256);
    let (withdrawals, _custody) = mpsc::unbounded_channel();
    let clock = Arc::new(crate::clock::test_clock::TestClock::default());
    let cfg = crate::sequencer::SequencerConfig {
        clock: clock.clone(),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(cfg, Arc::new(crate::tee::EnclaveKey::test_key()), stream, withdrawals).unwrap();
    let (permits, verdicts): (StdSender<Verdict>, StdReceiver<Verdict>) = channel();
    let verdicts = Mutex::new(verdicts);
    seq.pipeline_with_syncs(move |job| match verdicts.lock().unwrap().recv() {
        Ok(Verdict::Pass) => job.sync(),
        _ => anyhow::bail!("the disk failed"),
    })
    .unwrap();
    let weighting = dev_config().genesis.params.toxicity.notional.expect("the dev config weighs by notional");
    seq.carry_toxicity_notional(weighting).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    // One batch per tick, each waiting on the held sync, as `run` would stage them.
    for _ in 0..MAX_IN_FLIGHT {
        seq.sequence(Command::Tick).unwrap().unwrap();
        let release = seq.stage(Batch::default()).unwrap();
        seq.submit(release).unwrap();
    }
    let (tx, mut rx) = mpsc::channel(4);
    tx.send(Request::Query(Box::new(|_| {}))).await.unwrap();
    let held = timeout(Duration::from_millis(200), seq.next_request(&mut rx)).await;
    assert!(held.is_err(), "with {MAX_IN_FLIGHT} batches waiting, the next request waits too");
    permits.send(Verdict::Pass).unwrap();
    let taken = timeout(Duration::from_secs(10), seq.next_request(&mut rx)).await.expect("a finished sync frees it");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
}

#[tokio::test(start_paused = true)]
async fn a_sync_held_past_the_bound_pages_and_stamps_nothing_until_it_finishes() {
    let dir = scratch_dir("pipeline-held");
    let (stream, _) = broadcast::channel(256);
    let (withdrawals, _custody) = mpsc::unbounded_channel();
    let clock = Arc::new(crate::clock::test_clock::TestClock::default());
    let cfg = crate::sequencer::SequencerConfig {
        clock: clock.clone(),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(cfg, Arc::new(crate::tee::EnclaveKey::test_key()), stream, withdrawals).unwrap();
    let (permits, verdicts): (StdSender<Verdict>, StdReceiver<Verdict>) = channel();
    let verdicts = Mutex::new(verdicts);
    seq.pipeline_with_syncs(move |job| match verdicts.lock().unwrap().recv() {
        Ok(Verdict::Pass) => job.sync(),
        _ => anyhow::bail!("the disk failed"),
    })
    .unwrap();
    let weighting = dev_config().genesis.params.toxicity.notional.expect("the dev config weighs by notional");
    seq.carry_toxicity_notional(weighting).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let (_guard, mut alerts, mut resolved) = crate::test_support::capture_alerts_and_resolutions();
    seq.sequence(Command::Tick).unwrap().unwrap();
    let release = seq.stage(Batch::default()).unwrap();
    seq.submit(release).unwrap();
    let (tx, mut rx) = mpsc::channel(4);
    // Inside the bound the next request is taken while the sync runs.
    tx.send(Request::Query(Box::new(|_| {}))).await.unwrap();
    let taken = timeout(Duration::from_secs(1), seq.next_request(&mut rx)).await.expect("inside the bound");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
    assert!(alerts.try_recv().is_err(), "no page inside the bound");
    // Past it, the request waits behind the held sync and the hold pages once.
    tokio::time::advance(MAX_SYNC_HOLD).await;
    tx.send(Request::Query(Box::new(|_| {}))).await.unwrap();
    let held = timeout(MAX_SYNC_HOLD * 3, seq.next_request(&mut rx)).await;
    assert!(held.is_err(), "past the bound the sequencer takes no request");
    let page = alerts.try_recv().expect("the hold pages");
    assert_eq!(page.get("alert").map(String::as_str), Some("log_sync_held"));
    assert_eq!(page.get("level").map(String::as_str), Some("ERROR"), "a held sync is Critical");
    assert!(alerts.try_recv().is_err(), "one page per hold");
    // The sync finishes: the request is taken and the episode ends.
    permits.send(Verdict::Pass).unwrap();
    let taken = timeout(Duration::from_secs(1), seq.next_request(&mut rx)).await.expect("a finished sync frees it");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
    let end = resolved.try_recv().expect("the finished sync resolves the hold");
    assert_eq!(end.get("alert_resolved").map(String::as_str), Some("log_sync_held"));
}

#[tokio::test(flavor = "multi_thread")]
async fn batches_queued_behind_a_sync_share_the_next_one() {
    let node = Gated::start("pipeline-grouped");
    let first = node.tick().await;
    node.applied().await;
    node.sync_started();
    // The first sync is held, so these three queue behind it.
    let mut queued = Vec::new();
    for _ in 0..3 {
        queued.push(node.tick().await);
        node.applied().await;
    }
    node.permits.send(Verdict::Pass).unwrap();
    first.await.expect("the first batch is released by its own sync");
    // One more permit is all the three need: the thread syncs the last of them, which covers the rest.
    node.permits.send(Verdict::Pass).unwrap();
    for reply in queued {
        timeout(Duration::from_secs(10), reply).await.expect("released by the shared sync").unwrap();
    }
    assert_eq!(node.syncs.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_that_wrote_nothing_does_not_stand_in_for_the_sync_of_the_one_before_it() {
    let node = Gated::start("pipeline-empty-successor");
    let first = node.tick().await;
    node.applied().await;
    node.sync_started();
    // A batch that wrote, then one that wrote nothing but asks for a directory sync, both queued behind
    // the held sync.
    let written = node.tick().await;
    node.applied().await;
    let (reply, house) = oneshot::channel();
    node.tx.send(Request::Sync(reply)).await.unwrap();
    node.applied().await;
    for _ in 0..3 {
        node.permits.send(Verdict::Pass).unwrap();
    }
    first.await.unwrap();
    written.await.unwrap();
    timeout(Duration::from_secs(10), house).await.expect("the sync request is answered").unwrap();
    assert_eq!(node.syncs.load(Ordering::SeqCst), 3, "the batch that wrote has its own sync");
}

#[tokio::test(flavor = "multi_thread")]
async fn batches_that_only_answer_reads_take_no_pipeline_slot() {
    let node = Gated::start("pipeline-reads");
    let first = node.tick().await;
    node.applied().await;
    node.sync_started();
    // More read-only batches than the pipeline holds, all waiting on the one held sync.
    let mut answers = Vec::new();
    for _ in 0..=MAX_IN_FLIGHT {
        let (reply, answer) = oneshot::channel();
        node.tx.send(Request::HouseState(reply)).await.unwrap();
        timeout(Duration::from_secs(10), node.applied()).await.expect("the sequencer still takes requests");
        answers.push(answer);
    }
    // A price is still sequenced, and every answer goes out with the sync it waited for.
    let price = node.tick().await;
    timeout(Duration::from_secs(10), node.applied()).await.expect("a price is not held behind reads");
    node.permits.send(Verdict::Pass).unwrap();
    node.permits.send(Verdict::Pass).unwrap();
    first.await.unwrap();
    for answer in answers {
        timeout(Duration::from_secs(10), answer).await.expect("answered").unwrap();
    }
    price.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batchs_later_answers_go_out_with_its_release() {
    let node = Gated::start("pipeline-later");
    let first = node.tick().await;
    node.applied().await;
    node.sync_started();
    // Reads of the state the batch changed: payouts, claims, and a withdrawal reissue. None of them
    // wrote anything, so they ride the held batch's release.
    let account = AccountId([0xbb; 20]);
    let (payouts, mut payouts_rx) = oneshot::channel();
    node.tx.send(Request::AccountPayouts { account, after: None, limit: 10, reply: payouts }).await.unwrap();
    let (entitlement, mut entitlement_rx) = oneshot::channel();
    node.tx.send(Request::EntitlementClaim { account, reply: entitlement }).await.unwrap();
    let (distribution, mut distribution_rx) = oneshot::channel();
    node.tx.send(Request::DistributionClaim { account, reply: distribution }).await.unwrap();
    let (reissue, mut reissue_rx) = oneshot::channel();
    let reissue_req = Request::ReissueWithdrawal { account, nonce: 0, replaces: 0, screening: None, reply: reissue };
    node.tx.send(reissue_req).await.unwrap();
    node.applied().await;

    assert!(silent(&mut payouts_rx).await, "payouts wait for the batch's sync");
    assert!(silent(&mut entitlement_rx).await, "the entitlement claim waits");
    assert!(silent(&mut distribution_rx).await, "the distribution claim waits");
    assert!(silent(&mut reissue_rx).await, "the reissue waits");

    node.permits.send(Verdict::Pass).unwrap();
    first.await.unwrap();
    let payouts = timeout(Duration::from_secs(10), payouts_rx).await.expect("payouts answered").unwrap();
    assert!(!payouts.loaded, "mock custody holds no payout gate");
    let _ = timeout(Duration::from_secs(10), entitlement_rx).await.expect("the entitlement claim is answered");
    let _ = timeout(Duration::from_secs(10), distribution_rx).await.expect("the distribution claim is answered");
    let reissue = timeout(Duration::from_secs(10), reissue_rx).await.expect("the reissue is answered").unwrap();
    assert!(
        matches!(reissue, Reissue::Refused { .. }),
        "no screening evidence, so the reissue is refused: {reissue:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_signed_and_admin_requests_wait_for_their_batchs_sync() {
    let node = Gated::start("pipeline-refusals");
    let api = api_over(&node);
    let first = node.tick().await;
    node.applied().await;
    node.sync_started();
    let expires_at = api.clock.now_ms() + 60_000;

    // A signed order from an account the engine has never credited: refused on the speculative
    // engine's state, which the held sync could still take back.
    let stranger = SigningKey::from_slice(&[9; 32]).unwrap();
    let mut order = OrderRequest {
        account: eip712::address(stranger.verifying_key()),
        client_id: 1,
        market: 1,
        side: Side::Buy,
        qty: "0.1".parse().unwrap(),
        kind: OrderKind::Market { quoted: "100000".parse().unwrap(), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    let domain = eip712::domain(31337, AccountId([0; 20]));
    let signature = eip712::sign(&stranger, &order_hash(&domain, &order, 0, expires_at));
    order.auth = Some(RequestAuth { nonce: 0, expires_at, signature });
    // An admin command the operator key did not sign: refused on the same state.
    let forged = crate::admin::sign(
        &SigningKey::from_slice(&[8; 32]).unwrap(),
        &api.operator,
        AdminCommand::SetReduceOnly { market: None, reduce_only: true },
        0,
        expires_at,
    );
    let peer = MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)));
    let (orders_app, admin_app) = (crate::api::router(api.clone()).layer(peer), crate::api::router(api).layer(peer));
    let (signed_tx, mut signed_rx) = oneshot::channel();
    tokio::spawn(async move {
        let request = HttpRequest::post("/v1/orders")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&order).unwrap()))
            .unwrap();
        _ = signed_tx.send(orders_app.oneshot(request).await.unwrap().status());
    });
    let (admin_tx, mut admin_rx) = oneshot::channel();
    tokio::spawn(async move {
        let request = HttpRequest::post("/v1/admin")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&forged).unwrap()))
            .unwrap();
        _ = admin_tx.send(admin_app.oneshot(request).await.unwrap().status());
    });

    // The channel is FIFO: this query answering proves the API's reads above ran, so the refusals
    // are decided and now wait on the held sync's publish.
    node.applied().await;
    assert!(silent(&mut signed_rx).await, "the signed refusal waits for the batch's sync");
    assert!(silent(&mut admin_rx).await, "the admin refusal waits for it too");

    node.permits.send(Verdict::Pass).unwrap();
    first.await.unwrap();
    let signed = timeout(Duration::from_secs(10), signed_rx).await.expect("the signed refusal is answered").unwrap();
    assert_eq!(signed, StatusCode::FORBIDDEN);
    let admin = timeout(Duration::from_secs(10), admin_rx).await.expect("the admin refusal is answered").unwrap();
    assert_eq!(admin, StatusCode::UNAUTHORIZED);
}
