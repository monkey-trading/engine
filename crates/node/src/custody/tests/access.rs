//! Screened custody receipts through the production watcher: a hold survives restart and a later
//! clear source, and pending receipts wait for a declaration, a classification or a working source,
//! then credit once, locked until finality.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use alloy::primitives::B256;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use engine_core::Fixed;
use engine_core::access::{
    AccessEvidence, AccessPolicy, AccessReject, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus,
    Screening,
};
use engine_core::auth::{ClientAuth, RequestAuth};
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712::{self, Signature};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::access::AccessClient;
use crate::clock::now_ms;
use crate::config::{AccessSection, EvmCustodySection};
use crate::custody::evm::{Beneficiary, DepositWatcher, VaultDeposit};
use crate::custody::quorum::Quorum;
use crate::custody::stall::Polled;
use crate::custody::tests::fakes::{CHAIN_ID, FakeChain, Node, USDC, VAULT, deposit, evm_toml, operator, scratch};
use crate::log::LogEntry;
use crate::test_support::log_entries;

#[tokio::test]
async fn a_sanctioned_payer_receipt_is_held_uncredited_through_restart_and_a_later_clear_source() {
    for house in [false, true] {
        let dir = scratch(if house { "access-house-hold" } else { "access-trader-hold" });
        let (config, clear, calls, source_task) = source(answer).await;
        let policy = config.policy();
        let client = Arc::new(AccessClient::new(&config).unwrap());
        let node = Node::start_with_policy(&dir, Some(policy.clone()));
        activate(&node, policy.clone()).await;
        let trader = eip712::address(client_key().verifying_key());
        attest(&node, trader).await;
        let chains = chains(10);
        let beneficiary = if house { Beneficiary::House } else { Beneficiary::Trader(trader) };
        add(&chains, &funding(0, beneficiary, AccountId([0xbb; 20])));
        let mut watch = screened_watcher(&chains, &dir, &client);
        assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
        assert_eq!(held_reasons(&node).await, [AccessReject::PayerSanctioned]);
        assert_eq!(node.balance(trader).await, None);
        assert_eq!(node.read(|engine| engine.house_cash()).await, Fixed::ZERO);
        node.stop().await;
        clear.store(true, Ordering::SeqCst);
        let recovered = Node::start_with_policy(&dir, Some(policy));
        let mut watch = screened_watcher(&chains, &dir, &client);
        assert!(matches!(watch.poll(&recovered.tx).await, Ok(Polled::Read { caught_up: true })));
        assert_eq!(held_reasons(&recovered).await, [AccessReject::PayerSanctioned]);
        assert_eq!(recovered.balance(trader).await, None);
        let mut after = None;
        crate::custody::pending::retry_batch(Some(&client), &recovered.tx, &mut after).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a recorded hold is not screened again or released");
        assert_eq!(screened_receipts(&dir.join("log")), 1);
        recovered.stop().await;
        source_task.abort();
        let _ = source_task.await;
    }
}

#[tokio::test]
async fn outages_leave_receipts_pending_and_bounded_retries_credit_each_once_locked_until_finality() {
    use crate::custody::pending::{RETRY_BATCH, retry_batch};
    let dir = scratch("access-outage-retry");
    let (config, clear, calls, source_task) = source(outage_answer).await;
    let client = Arc::new(AccessClient::new(&config).unwrap());
    let node = Node::start_with_policy(&dir, Some(config.policy()));
    let trader = eip712::address(client_key().verifying_key());
    let chains = chains(RETRY_BATCH as u64 + 1);
    for index in 0..=RETRY_BATCH as u64 {
        add(&chains, &funding(index, Beneficiary::Trader(trader), trader));
    }
    let mut watch = screened_watcher(&chains, &dir, &client);
    // Before startup activation commits, the build still screens; the engine refuses the receipt.
    assert!(watch.poll(&node.tx).await.is_err());
    assert_eq!(node.balance(trader).await, None);
    assert!(node.read(|engine| engine.held_deposits().is_empty() && engine.pending_deposits().is_empty()).await);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the receipt was screened before the refusal");
    let unscreened = log_entries(&dir.join("log")).into_iter().any(|entry| {
        matches!(entry, LogEntry::Command(command) if matches!(command.sequenced.cmd, Command::CustodyDeposit(_)))
    });
    assert!(!unscreened, "the build never sequences an unscreened receipt");
    activate(&node, config.policy()).await;
    attest(&node, trader).await;
    assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
    assert_eq!(node.read(|engine| engine.pending_deposits().len()).await, RETRY_BATCH + 1);
    assert_eq!(node.balance(trader).await, None);
    node.stop().await;
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let recovered = Node::start_with_policy(&dir, Some(config.policy()));
    let mut watch = screened_watcher(&chains, &dir, &client);
    assert!(matches!(watch.poll(&recovered.tx).await, Ok(Polled::Read { caught_up: true })));
    assert_eq!(calls.load(Ordering::SeqCst), RETRY_BATCH + 2, "a rescan does not screen recorded receipts again");
    let mut after = None;
    let turn = retry_batch(Some(&client), &recovered.tx, &mut after).await.unwrap();
    assert!(turn.failing && !turn.unsettled, "an outage fails the turn without backing off");
    assert_eq!(recovered.balance(trader).await, None);
    clear.store(true, Ordering::SeqCst);
    let turn = retry_batch(Some(&client), &recovered.tx, &mut after).await.unwrap();
    assert!(!turn.failing && !turn.unsettled, "a recovered source settles the whole turn");
    assert_eq!(recovered.read(|engine| engine.pending_deposits().len()).await, 1, "one turn is bounded");
    let credited = Fixed::from_int(50 * RETRY_BATCH as i64);
    assert_eq!(recovered.balance(trader).await, Some(credited));
    let locked = recovered.read(move |engine| engine.accounts()[&trader].unfinalised_deposits).await;
    assert_eq!(locked, credited, "principal credited before finality is locked until it");
    retry_batch(Some(&client), &recovered.tx, &mut after).await.unwrap();
    assert_eq!(recovered.balance(trader).await, Some(Fixed::from_int(50 * (RETRY_BATCH as i64 + 1))));
    assert!(recovered.read(|engine| engine.pending_deposits().is_empty() && engine.held_deposits().is_empty()).await);
    let calls_before = calls.load(Ordering::SeqCst);
    retry_batch(Some(&client), &recovered.tx, &mut after).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), calls_before);
    assert_eq!(screened_receipts(&dir.join("log")), RETRY_BATCH + 2, "one inactive refusal and nine pending inputs");
    assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), 1);
    recovered.stop().await;
    source_task.abort();
    let _ = source_task.await;
}

#[tokio::test]
async fn retries_sequence_nothing_while_the_source_fails_and_back_off_while_its_evidence_does_not_verify() {
    use crate::custody::pending::{MAX_RETRY_INTERVAL, next_wait, retry_batch};
    let dir = scratch("access-retry-bounds");
    let (config, clear, calls, source_task) = source(outage_answer).await;
    let client = Arc::new(AccessClient::new(&config).unwrap());
    // The log records another authority, so every answer the source signs fails verification.
    let authority = SigningKey::from_slice(&[0x46; 32]).unwrap();
    let policy = AccessPolicy { source: eip712::address(authority.verifying_key()), ..config.policy() };
    let node = Node::start_with_policy(&dir, Some(policy.clone()));
    activate(&node, policy).await;
    let trader = eip712::address(client_key().verifying_key());
    attest_with(&node, trader, &authority).await;
    let chains = chains(2);
    for id in 0..2 {
        add(&chains, &funding(id, Beneficiary::Trader(trader), trader));
    }
    let mut watch = screened_watcher(&chains, &dir, &client);
    assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
    assert_eq!(node.read(|engine| engine.pending_deposits().len()).await, 2);
    let mut after = None;
    let seq = node.last_seq().await;
    let turn = retry_batch(Some(&client), &node.tx, &mut after).await.unwrap();
    assert!(turn.failing && !turn.unsettled);
    assert_eq!(node.last_seq().await, seq, "a failing source sequences no retry");
    assert_eq!(calls.load(Ordering::SeqCst), 4, "both pending receipts were screened again");
    clear.store(true, Ordering::SeqCst);
    let turn = retry_batch(Some(&client), &node.tx, &mut after).await.unwrap();
    assert!(turn.failing && turn.unsettled);
    assert_eq!(node.last_seq().await, seq.map(|seq| seq + 1), "the turn ends at the first retry left pending");
    assert_eq!(node.read(|engine| engine.pending_deposits().len()).await, 2);
    assert!(node.read(|engine| engine.held_deposits().is_empty()).await, "unverified evidence holds nothing");
    let backoff: Vec<_> =
        std::iter::successors(Some(Duration::from_secs(5)), |wait| Some(next_wait(*wait, false))).take(8).collect();
    assert_eq!(backoff.last(), Some(&MAX_RETRY_INTERVAL));
    assert!(backoff.windows(2).all(|pair| pair[1] == (pair[0] * 2).min(MAX_RETRY_INTERVAL)));
    assert_eq!(next_wait(MAX_RETRY_INTERVAL, true), Duration::from_secs(5));
    node.stop().await;
    source_task.abort();
    let _ = source_task.await;
}

#[tokio::test]
async fn retries_wait_for_a_declaration_and_a_classification_without_sequencing_anything() {
    use crate::custody::pending::retry_batch;
    let dir = scratch("access-curable-retry");
    let (config, classified, calls, source_task) = source(classifying_answer).await;
    let client = Arc::new(AccessClient::new(&config).unwrap());
    let node = Node::start_with_policy(&dir, Some(config.policy()));
    activate(&node, config.policy()).await;
    let trader = eip712::address(client_key().verifying_key());
    let chains = chains(1);
    add(&chains, &funding(0, Beneficiary::Trader(trader), trader));
    let mut watch = screened_watcher(&chains, &dir, &client);
    assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
    assert_eq!(
        node.read(|engine| engine.pending_deposits().len()).await,
        1,
        "an undeclared, unclassified receipt waits pending"
    );
    let (mut after, seq) = (None, node.last_seq().await);
    let turn = retry_batch(Some(&client), &node.tx, &mut after).await.unwrap();
    assert!(!turn.failing && !turn.unsettled);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "an undeclared account's receipt is not screened again");
    assert_eq!(node.last_seq().await, seq);
    attest(&node, trader).await;
    let seq = node.last_seq().await;
    let turn = retry_batch(Some(&client), &node.tx, &mut after).await.unwrap();
    assert!(!turn.failing && !turn.unsettled, "an unknown classification is not a source failure");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(node.last_seq().await, seq, "a screening that still cannot classify the payer is not sequenced");
    classified.store(true, Ordering::SeqCst);
    retry_batch(Some(&client), &node.tx, &mut after).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(node.balance(trader).await, Some(Fixed::from_int(50)));
    assert!(node.read(|engine| engine.pending_deposits().is_empty() && engine.held_deposits().is_empty()).await);
    node.stop().await;
    source_task.abort();
    let _ = source_task.await;
}

#[test]
fn a_failing_retry_turn_raises_one_alert_episode_until_a_turn_succeeds() {
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let mut alert = crate::custody::pending::RetryAlert::default();
    alert.note(false);
    assert_eq!(logs.count("access_deposit_retry_failed"), 0, "a quiet turn starts no episode");
    for _ in 0..3 {
        alert.note(true);
    }
    assert_eq!(logs.count("alert=\"access_deposit_retry_failed\""), 1);
    alert.note(false);
    alert.note(false);
    assert_eq!(logs.count("alert_resolved=\"access_deposit_retry_failed\""), 1);
    alert.note(true);
    assert_eq!(logs.count("alert=\"access_deposit_retry_failed\""), 2, "a new failure starts a new episode");
}

async fn outage_answer(
    State((clear, calls)): State<(Arc<AtomicBool>, Arc<AtomicUsize>)>,
    Json(request): Json<Value>,
) -> Result<Json<Screening>, axum::http::StatusCode> {
    if !clear.load(Ordering::SeqCst) {
        calls.fetch_add(1, Ordering::SeqCst);
        return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
    Ok(answer(State((clear, calls)), Json(request)).await)
}
fn client_key() -> SigningKey {
    SigningKey::from_slice(&[1; 32]).unwrap()
}

async fn activate(node: &Node, policy: AccessPolicy) {
    for command in [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(operator()),
        AdminCommand::SetAccessPolicy(policy),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V24 },
    ] {
        assert_eq!(node.submit(Command::Admin(command)).await, [Event::AdminApplied]);
    }
}

async fn attest(node: &Node, account: AccountId) {
    attest_with(node, account, &source_key()).await;
}

/// Records `account`'s declaration with allowed, clear evidence signed by `authority`.
async fn attest_with(node: &Node, account: AccountId, authority: &SigningKey) {
    let operator = operator();
    let nonce = 0;
    let expires_at = now_ms() + 60_000;
    let hash = eip712::eligibility_hash(
        &eip712::domain(operator.chain_id, operator.vault),
        account,
        EligibilityClass::Professional,
        nonce,
        expires_at,
    );
    let request = EligibilityRequest {
        account,
        class: EligibilityClass::Professional,
        auth: RequestAuth { nonce, expires_at, signature: eip712::sign(&client_key(), &hash) },
    };
    let query = serde_json::json!({ "request_hash": hash, "account": account, "payer": null });
    let screening = Screening {
        source: eip712::address(authority.verifying_key()),
        jurisdiction: Jurisdiction::Allowed,
        ..signed_answer(&query, SanctionsStatus::Clear)
    };
    let evidence = AccessEvidence::Screened(signed_by(authority, screening));
    assert_eq!(
        node.submit(Command::AttestEligibility { request, evidence }).await,
        [Event::EligibilityAttested { account, class: EligibilityClass::Professional }]
    );
}

/// How many screened receipts the log in `dir` holds.
fn screened_receipts(dir: &Path) -> usize {
    log_entries(dir)
        .into_iter()
        .filter(|entry| {
            matches!(entry, LogEntry::Command(command)
                if matches!(command.sequenced.cmd, Command::AccessCustodyDeposit { .. }))
        })
        .count()
}

/// The reasons of every held deposit, in reference order.
async fn held_reasons(node: &Node) -> Vec<AccessReject> {
    node.read(|engine| engine.held_deposits().values().map(|hold| hold.reason).collect()).await
}

/// A loopback source serving `handler`, whose flag and call count the test controls.
async fn source<H, T>(handler: H) -> (AccessSection, Arc<AtomicBool>, Arc<AtomicUsize>, JoinHandle<()>)
where
    H: axum::handler::Handler<T, (Arc<AtomicBool>, Arc<AtomicUsize>)>,
    T: 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config: AccessSection = toml::from_str(&format!(
        "source='{}'\nendpoint='http://{}'\ntimeout='2s'\nmax_age='60s'\nmax_response_bytes=4096\nmax_in_flight=2",
        eip712::address(source_key().verifying_key()),
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route("/", post(handler)).with_state((flag.clone(), calls.clone()));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (config, flag, calls, task)
}

/// Three agreeing providers whose head and confirmation tag are `head`, with no withdrawal finality.
fn chains(head: u64) -> [FakeChain; 3] {
    std::array::from_fn(|_| {
        let chain = FakeChain::default();
        {
            let mut state = chain.0.lock().unwrap();
            state.head = head;
            state.final_block = Some(head);
        }
        chain
    })
}

fn add(chains: &[FakeChain; 3], deposit: &VaultDeposit) {
    for chain in chains {
        chain.add(deposit.clone());
    }
}

/// 50 USDC from `payer` in block `id + 1`, in its own transaction.
fn funding(id: u64, beneficiary: Beneficiary, payer: AccountId) -> VaultDeposit {
    VaultDeposit {
        tx_hash: B256::repeat_byte(id as u8 + 1),
        payer: Some(payer),
        ..deposit(id, id + 1, beneficiary, 50 * USDC)
    }
}

/// The production receipt watcher, which screens every new receipt with its own source capacity.
fn screened_watcher(
    chains: &[FakeChain; 3],
    dir: &Path,
    client: &Arc<AccessClient>,
) -> DepositWatcher<Quorum<FakeChain>> {
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(dir, 100)).unwrap();
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    let mut watcher = DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap();
    watcher.access = Some(client.clone());
    watcher
}

async fn answer(
    State((clear, calls)): State<(Arc<AtomicBool>, Arc<AtomicUsize>)>,
    Json(request): Json<Value>,
) -> Json<Screening> {
    calls.fetch_add(1, Ordering::SeqCst);
    let payer_status = if clear.load(Ordering::SeqCst) { SanctionsStatus::Clear } else { SanctionsStatus::Sanctioned };
    Json(signed_answer(&request, payer_status))
}

/// Answers `unknown` for the payer until `classified`, then clear.
async fn classifying_answer(
    State((classified, calls)): State<(Arc<AtomicBool>, Arc<AtomicUsize>)>,
    Json(request): Json<Value>,
) -> Json<Screening> {
    calls.fetch_add(1, Ordering::SeqCst);
    let payer_status =
        if classified.load(Ordering::SeqCst) { SanctionsStatus::Clear } else { SanctionsStatus::Unknown };
    Json(signed_answer(&request, payer_status))
}

/// A clear account and `payer_status` for the query's subjects, signed by the source key now.
fn signed_answer(request: &Value, payer_status: SanctionsStatus) -> Screening {
    signed(Screening {
        source: eip712::address(source_key().verifying_key()),
        request_hash: serde_json::from_value(request["request_hash"].clone()).unwrap(),
        observed_at: now_ms(),
        account: serde_json::from_value(request["account"].clone()).unwrap(),
        payer: serde_json::from_value(request["payer"].clone()).unwrap(),
        jurisdiction: Jurisdiction::Unknown,
        account_status: SanctionsStatus::Clear,
        payer_status,
        signature: Signature::ZERO,
    })
}

/// `screening` signed by the source key under the operator's domain.
fn signed(screening: Screening) -> Screening {
    signed_by(&source_key(), screening)
}

/// The deterministic screening authority the access fixtures share with the dev config.
fn source_key() -> SigningKey {
    SigningKey::from_slice(&[0x77; 32]).unwrap()
}

/// `screening` signed by `key` under the operator's domain.
fn signed_by(key: &SigningKey, mut screening: Screening) -> Screening {
    let operator = operator();
    screening.signature =
        eip712::sign(key, &eip712::screening_hash(&eip712::domain(operator.chain_id, operator.vault), &screening));
    screening
}

#[path = "access/replacements.rs"]
mod replacements;
