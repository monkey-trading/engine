//! Pending deposits whose screening evidence does not verify fail their retries, and undeclared
//! accounts' pending deposits are re-screened for a confirmed sanction only.

use super::*;
use crate::custody::pending::{UNDECLARED_BATCH, rescreen_undeclared, retry_batch, undeclared_page};
use std::sync::atomic::AtomicU8;

/// What the loopback source answers.
#[derive(Clone, Copy)]
pub(super) enum Mode {
    Down = 0,
    Unknown,
    Sanctioned,
    UnknownWrongSigner,
    UnknownStale,
    SanctionedWrongSigner,
    SanctionedStale,
    Clear,
}

impl Mode {
    const ALL: [Mode; 8] = [
        Mode::Down,
        Mode::Unknown,
        Mode::Sanctioned,
        Mode::UnknownWrongSigner,
        Mode::UnknownStale,
        Mode::SanctionedWrongSigner,
        Mode::SanctionedStale,
        Mode::Clear,
    ];
}

/// A loopback source answering as `mode` says, whose mode and call count the test controls.
pub(super) async fn moded_source() -> (AccessSection, Arc<AtomicU8>, Arc<AtomicUsize>, JoinHandle<()>) {
    async fn handler(
        State((mode, calls)): State<(Arc<AtomicU8>, Arc<AtomicUsize>)>,
        Json(request): Json<Value>,
    ) -> Result<Json<Screening>, axum::http::StatusCode> {
        calls.fetch_add(1, Ordering::SeqCst);
        let mode = Mode::ALL[mode.load(Ordering::SeqCst) as usize];
        let (status, wrong, stale) = match mode {
            Mode::Down => return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE),
            Mode::Unknown => (SanctionsStatus::Unknown, false, false),
            Mode::Sanctioned => (SanctionsStatus::Sanctioned, false, false),
            Mode::UnknownWrongSigner => (SanctionsStatus::Unknown, true, false),
            Mode::UnknownStale => (SanctionsStatus::Unknown, false, true),
            Mode::SanctionedWrongSigner => (SanctionsStatus::Sanctioned, true, false),
            Mode::SanctionedStale => (SanctionsStatus::Sanctioned, false, true),
            Mode::Clear => (SanctionsStatus::Clear, false, false),
        };
        let mut screening = signed_answer(&request, status);
        if stale {
            screening.observed_at -= 120_000;
        }
        let key = if wrong { SigningKey::from_slice(&[0x46; 32]).unwrap() } else { source_key() };
        Ok(Json(signed_by(&key, screening)))
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config: AccessSection = toml::from_str(&format!(
        "source='{}'\nendpoint='http://{}'\ntimeout='2s'\nmax_age='60s'\nmax_response_bytes=4096\nmax_in_flight=2",
        eip712::address(source_key().verifying_key()),
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let (mode, calls) = (Arc::new(AtomicU8::new(Mode::Down as u8)), Arc::new(AtomicUsize::new(0)));
    let app = Router::new().route("/", post(handler)).with_state((mode.clone(), calls.clone()));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (config, mode, calls, task)
}

/// A node with `count` pending deposits of one trader, taken while the source was down.
pub(super) struct Fixture {
    pub(super) node: Node,
    pub(super) client: Arc<AccessClient>,
    pub(super) mode: Arc<AtomicU8>,
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) source: JoinHandle<()>,
    pub(super) chains: [FakeChain; 3],
    pub(super) watch: DepositWatcher<Quorum<FakeChain>>,
}

pub(super) async fn pending_node(name: &str, count: u64, declared: bool) -> Fixture {
    let dir = scratch(name);
    let (config, mode, calls, source) = moded_source().await;
    let client = Arc::new(AccessClient::new(&config).unwrap());
    let node = Node::start_with_policy(&dir, Some(config.policy()));
    activate(&node, config.policy()).await;
    let trader = eip712::address(client_key().verifying_key());
    if declared {
        attest(&node, trader).await;
    }
    let chains = chains(count + 1);
    for id in 0..count {
        add(&chains, &funding(id, Beneficiary::Trader(trader), AccountId([0xbb; 20])));
    }
    let mut watch = screened_watcher(&chains, &dir, &client);
    assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
    assert_eq!(node.read(|engine| engine.pending_deposits().len()).await, count as usize);
    Fixture { node, client, mode, calls, source, chains, watch }
}

#[tokio::test]
async fn unknown_evidence_that_does_not_verify_fails_the_retry_instead_of_waiting_quietly() {
    let Fixture { node, client, mode, calls, source: task, .. } =
        pending_node("access-unverified-unknown", 1, true).await;
    for (unverified, name) in [(Mode::UnknownWrongSigner, "wrong signer"), (Mode::UnknownStale, "stale")] {
        mode.store(unverified as u8, Ordering::SeqCst);
        let (mut after, seq, before) = (None, node.last_seq().await, calls.load(Ordering::SeqCst));
        let turn = retry_batch(Some(&client), &node.tx, &mut after).await.unwrap();
        assert!(turn.failing && !turn.unsettled, "{name} evidence fails the turn");
        assert_eq!(calls.load(Ordering::SeqCst), before + 1);
        assert_eq!(node.last_seq().await, seq, "{name} evidence sequences nothing");
    }
    mode.store(Mode::Unknown as u8, Ordering::SeqCst);
    let mut after = None;
    let turn = retry_batch(Some(&client), &node.tx, &mut after).await.unwrap();
    assert!(!turn.failing && !turn.unsettled, "verified unknown evidence still waits quietly");
    node.stop().await;
    task.abort();
}

#[tokio::test]
async fn a_late_confirmed_sanction_holds_a_deposit_pending_only_for_a_missing_declaration() {
    let Fixture { node, client, mode, calls, source: task, .. } =
        pending_node("access-undeclared-sanction", 1, false).await;
    let trader = eip712::address(client_key().verifying_key());
    let mut after = None;
    let seq = node.last_seq().await;
    retry_batch(Some(&client), &node.tx, &mut after).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the retry worker skips an undeclared account");
    // Nothing but a verified confirmed sanction is sequenced.
    for mode_now in
        [Mode::Down, Mode::Unknown, Mode::UnknownWrongSigner, Mode::SanctionedWrongSigner, Mode::SanctionedStale]
    {
        mode.store(mode_now as u8, Ordering::SeqCst);
        let mut cursor = None;
        rescreen_undeclared(Some(&client), &node.tx, &mut cursor).await.unwrap();
        assert_eq!(node.last_seq().await, seq, "the source answering as {} sequences nothing", mode_now as u8);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    mode.store(Mode::Sanctioned as u8, Ordering::SeqCst);
    let mut cursor = None;
    rescreen_undeclared(Some(&client), &node.tx, &mut cursor).await.unwrap();
    assert_eq!(held_reasons(&node).await, [AccessReject::PayerSanctioned]);
    assert!(node.read(|engine| engine.pending_deposits().is_empty()).await);
    assert_eq!(node.balance(trader).await, None);
    node.stop().await;
    task.abort();
}

#[tokio::test]
async fn an_undeclared_rescreen_is_bounded_and_resumes_where_it_stopped() {
    let count = UNDECLARED_BATCH as u64 + 2;
    let Fixture { node, client, mode, calls, source: task, .. } =
        pending_node("access-undeclared-bound", count, false).await;
    mode.store(Mode::Sanctioned as u8, Ordering::SeqCst);
    let before = calls.load(Ordering::SeqCst);
    let mut cursor = None;
    rescreen_undeclared(Some(&client), &node.tx, &mut cursor).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst) - before, UNDECLARED_BATCH, "one turn screens a bounded batch");
    assert_eq!(node.read(|engine| engine.pending_deposits().len()).await, 2);
    rescreen_undeclared(Some(&client), &node.tx, &mut cursor).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst) - before, UNDECLARED_BATCH + 2, "the next turn resumes, repeating none");
    assert!(node.read(|engine| engine.pending_deposits().is_empty()).await);
    node.stop().await;
    task.abort();
}

#[tokio::test]
async fn an_undeclared_scan_examines_a_bounded_number_of_pending_deposits() {
    let Fixture { node, source: task, .. } = pending_node("access-undeclared-scan", 5, false).await;
    let (found, last) = node.read(|engine| undeclared_page(engine, "", 2, 8)).await;
    assert_eq!(found.len(), 2, "two examined, both undeclared");
    assert_eq!(last.as_deref(), found.last().map(|(reference, _)| reference.as_str()));
    let (found, _) = node.read(move |engine| undeclared_page(engine, "", 100, 3)).await;
    assert_eq!(found.len(), 3, "a page is bounded by its batch");
    let (found, last) = node.read(|engine| undeclared_page(engine, "", 100, 100)).await;
    assert_eq!(found.len(), 5);
    assert!(last.is_some());
    node.stop().await;
    task.abort();
}
