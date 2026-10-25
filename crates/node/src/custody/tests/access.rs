//! A public hold survives cursor loss, restart, source recovery and post-finality movement.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
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

use crate::access::AccessClient;
use crate::clock::now_ms;
use crate::config::AccessSection;
use crate::custody::evm::Beneficiary;
use crate::custody::tests::fakes::{FakeChain, Node, USDC, deposit, operator, scratch, stored_cursor, watcher};
use crate::log::LogEntry;
use crate::test_support::log_entries;

#[tokio::test]
async fn a_sanctioned_payer_deposit_for_an_eligible_account_stays_held_through_restart_and_cursor_loss() {
    for house in [false, true] {
        let dir = scratch(if house { "access-house-hold" } else { "access-trader-hold" });
        let chain = FakeChain::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config: AccessSection = toml::from_str(&format!(
            "source='{}'\nendpoint='http://{}'\ntimeout='2s'\nmax_age='5s'\nmax_response_bytes=4096\nmax_in_flight=2",
            eip712::address(source_key().verifying_key()),
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let policy = config.policy();
        let clear = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route("/", post(answer)).with_state((clear.clone(), calls.clone()));
        let source_task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = Arc::new(AccessClient::new(&config).unwrap());
        let node = Node::start_with_policy(&dir, Some(policy.clone()));
        activate(&node, policy.clone()).await;
        let trader = eip712::address(client_key().verifying_key());
        attest(&node, trader).await;
        let mut funding =
            deposit(0, 1, if house { Beneficiary::House } else { Beneficiary::Trader(trader) }, 50 * USDC);
        funding.payer = Some(AccountId([0xbb; 20]));
        chain.add(funding.clone());
        chain.set_final(Some(1));
        let mut watch = watcher(&chain, &dir, 1000, node.last_seq().await);
        watch.access = Some(client.clone());
        assert!(matches!(watch.poll(&node.tx).await, Ok(crate::custody::stall::Polled::Read { caught_up: true })));
        assert_eq!(stored_cursor(&dir).next_deposit_id, 1, "the durable hold lets unrelated deposits advance");
        assert_eq!(node.balance(trader).await, None);
        assert_eq!(node.read(|engine| engine.house_cash()).await, engine_core::Fixed::ZERO);
        let reference = funding.chain_ref();
        let held_ref = reference.clone();
        assert_eq!(
            node.read(move |engine| engine.held_deposits()[&held_ref].reason).await,
            AccessReject::PayerSanctioned
        );
        node.stop().await;
        let before = access_deposits(&dir.join("log"));
        assert_eq!(before, 1);
        std::fs::remove_file(dir.join("cursor.json")).unwrap();
        clear.store(true, Ordering::SeqCst);
        let recovered = Node::start_with_policy(&dir, Some(policy));
        let mut watch = watcher(&chain, &dir, 1000, recovered.last_seq().await);
        watch.access = Some(client);
        assert!(matches!(watch.poll(&recovered.tx).await, Ok(crate::custody::stall::Polled::Read { caught_up: true })));
        assert_eq!(recovered.balance(trader).await, None);
        let held_ref = reference.clone();
        assert_eq!(
            recovered.read(move |engine| engine.held_deposits()[&held_ref].reason).await,
            AccessReject::PayerSanctioned
        );
        assert_eq!(access_deposits(&dir.join("log")), before);
        let mut after = None;
        crate::custody::pending::retry_batch(watch.access.as_deref(), &recovered.tx, &mut after).await.unwrap();
        let moved = crate::custody::evm::VaultDeposit { log_index: funding.log_index + 1, ..funding };
        assert_eq!(crate::custody::credit::moved_credits(&recovered.tx, &[moved]).await.unwrap(), [reference]);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a held reference is not rescreened or released");
        recovered.stop().await;
        source_task.abort();
        let _ = source_task.await;
    }
}

#[tokio::test]
async fn the_build_requires_screened_ingress_even_before_startup_activation() {
    let dir = scratch("access-before-activation");
    let node = Node::start(&dir);
    let funding = deposit(0, 1, Beneficiary::Trader(AccountId([1; 20])), 50 * USDC);
    let result = crate::custody::credit::credit(&node.tx, &funding, None, true).await;
    assert!(matches!(result, Err(crate::custody::stall::PollError::Stalled(_))));
    assert_eq!(node.balance(AccountId([1; 20])).await, None);
    assert!(node.read(|engine| engine.held_deposits().is_empty()).await);
    let commands = log_entries(&dir.join("log"));
    assert!(commands.iter().any(|entry| matches!(entry, LogEntry::Command(command)
        if matches!(command.sequenced.cmd, Command::AccessDeposit { .. }))));
    assert!(!commands.iter().any(|entry| matches!(entry, LogEntry::Command(command)
        if matches!(command.sequenced.cmd, Command::Deposit { .. }))));
    node.stop().await;
}

fn source_key() -> SigningKey {
    SigningKey::from_slice(&[0x77; 32]).unwrap()
}

#[tokio::test]
async fn outages_advance_the_cursor_and_replay_pending_work_for_bounded_exactly_once_credit() {
    use crate::custody::pending::{RETRY_BATCH, retry_batch};
    let dir = scratch("access-outage-retry");
    let chain = FakeChain::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config: AccessSection = toml::from_str(&format!(
        "source='{}'\nendpoint='http://{}'\ntimeout='2s'\nmax_age='60s'\nmax_response_bytes=4096\nmax_in_flight=2",
        eip712::address(source_key().verifying_key()),
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let clear = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route("/", post(outage_answer)).with_state((clear.clone(), calls.clone()));
    let source = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = Arc::new(AccessClient::new(&config).unwrap());
    let node = Node::start_with_policy(&dir, Some(config.policy()));
    let trader = eip712::address(client_key().verifying_key());
    for index in 0..=RETRY_BATCH {
        let mut funding = deposit(index as u64, index as u64 + 1, Beneficiary::Trader(trader), 50 * USDC);
        funding.payer = Some(trader);
        chain.add(funding);
    }
    chain.set_final(Some(RETRY_BATCH as u64 + 1));
    let watcher_config = toml::from_str(&crate::custody::tests::fakes::evm_toml(&dir, 1000)).unwrap();
    let mut watch = crate::custody::evm::DepositWatcher::start(
        chain.clone(),
        &watcher_config,
        crate::custody::tests::fakes::CHAIN_ID,
        crate::custody::tests::fakes::VAULT,
        node.last_seq().await,
    )
    .unwrap();
    watch.access = Some(client.clone());
    assert!(matches!(watch.poll(&node.tx).await, Err(crate::custody::stall::PollError::Stalled(_))));
    assert_eq!(node.balance(trader).await, None);
    assert!(node.read(|engine| engine.held_deposits().is_empty() && engine.pending_deposits().is_empty()).await);
    assert_eq!(stored_cursor(&dir).next_deposit_id, 0, "inactive access rules cannot durably dispose of funding");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the actual constructor screens before activation");
    activate(&node, config.policy()).await;
    attest(&node, trader).await;
    assert!(matches!(watch.poll(&node.tx).await, Ok(crate::custody::stall::Polled::Read { caught_up: true })));
    assert_eq!(stored_cursor(&dir).next_deposit_id, RETRY_BATCH as u64 + 1);
    assert_eq!(node.read(|engine| engine.pending_deposits().len()).await, RETRY_BATCH + 1);
    assert_eq!(node.balance(trader).await, None);
    let mut moved = deposit(0, 1, Beneficiary::Trader(trader), 50 * USDC);
    let original = moved.chain_ref();
    moved.log_index += 1;
    assert_eq!(crate::custody::credit::moved_credits(&node.tx, &[moved]).await.unwrap(), [original]);
    node.stop().await;
    std::fs::remove_file(dir.join("cursor.json")).unwrap();
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let recovered = Node::start_with_policy(&dir, Some(config.policy()));
    let mut watch = watcher(&chain, &dir, 1000, recovered.last_seq().await);
    watch.access = Some(client.clone());
    assert!(matches!(watch.poll(&recovered.tx).await, Ok(crate::custody::stall::Polled::Read { caught_up: true })));
    assert_eq!(calls.load(Ordering::SeqCst), RETRY_BATCH + 2, "rescan does not repeat pending ingestion");
    let mut after = None;
    let turn = retry_batch(Some(&client), &recovered.tx, &mut after).await.unwrap();
    assert!(turn.failing && !turn.unsettled, "an outage fails the turn without backing off");
    assert_eq!(recovered.balance(trader).await, None);
    clear.store(true, Ordering::SeqCst);
    let turn = retry_batch(Some(&client), &recovered.tx, &mut after).await.unwrap();
    assert!(!turn.failing && !turn.unsettled, "a recovered source settles the whole turn");
    assert_eq!(
        recovered.read(|engine| engine.pending_deposits().len()).await,
        1,
        "one turn is bounded even after recovery"
    );
    assert_eq!(recovered.balance(trader).await, Some(engine_core::Fixed::from_int(50 * RETRY_BATCH as i64)));
    retry_batch(Some(&client), &recovered.tx, &mut after).await.unwrap();
    assert_eq!(recovered.balance(trader).await, Some(engine_core::Fixed::from_int(50 * (RETRY_BATCH as i64 + 1))));
    assert_eq!(recovered.read(|engine| engine.pending_deposits().len()).await, 0);
    assert!(recovered.read(|engine| engine.held_deposits().is_empty()).await);
    let calls_before = calls.load(Ordering::SeqCst);
    retry_batch(Some(&client), &recovered.tx, &mut after).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), calls_before);
    assert_eq!(access_deposits(&dir.join("log")), RETRY_BATCH + 2, "one inactive refusal and nine pending inputs");
    assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), 1);
    recovered.stop().await;
    source.abort();
    let _ = source.await;
}

#[tokio::test]
async fn retries_sequence_nothing_while_the_source_fails_and_back_off_while_its_evidence_does_not_verify() {
    use crate::custody::pending::{MAX_RETRY_INTERVAL, next_wait, retry_batch};
    let dir = scratch("access-retry-bounds");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config: AccessSection = toml::from_str(&format!(
        "source='{}'\nendpoint='http://{}'\ntimeout='2s'\nmax_age='60s'\nmax_response_bytes=4096\nmax_in_flight=2",
        eip712::address(source_key().verifying_key()),
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let clear = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route("/", post(outage_answer)).with_state((clear.clone(), calls.clone()));
    let source = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = AccessClient::new(&config).unwrap();
    // The log records another authority, so every answer the source signs fails verification.
    let authority = SigningKey::from_slice(&[0x46; 32]).unwrap();
    let policy = AccessPolicy { source: eip712::address(authority.verifying_key()), ..config.policy() };
    let node = Node::start_with_policy(&dir, Some(policy.clone()));
    activate(&node, policy).await;
    let trader = eip712::address(client_key().verifying_key());
    attest_with(&node, trader, &authority).await;
    for id in 0..2 {
        let mut funding = deposit(id, id + 1, Beneficiary::Trader(trader), 50 * USDC);
        funding.payer = Some(trader);
        assert!(crate::custody::credit::credit(&node.tx, &funding, Some(&client), true).await.is_ok());
    }
    assert_eq!(node.read(|engine| engine.pending_deposits().len()).await, 2);
    let mut after = None;
    let seq = node.last_seq().await;
    let turn = retry_batch(Some(&client), &node.tx, &mut after).await.unwrap();
    assert!(turn.failing && !turn.unsettled);
    assert_eq!(node.last_seq().await, seq, "a failing source sequences no retry");
    assert_eq!(calls.load(Ordering::SeqCst), 4, "both pending deposits were screened again");
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
    source.abort();
    let _ = source.await;
}

#[tokio::test]
async fn retries_wait_for_a_declaration_and_a_classification_without_sequencing_anything() {
    use crate::custody::pending::retry_batch;
    let dir = scratch("access-curable-retry");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config: AccessSection = toml::from_str(&format!(
        "source='{}'\nendpoint='http://{}'\ntimeout='2s'\nmax_age='60s'\nmax_response_bytes=4096\nmax_in_flight=2",
        eip712::address(source_key().verifying_key()),
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let classified = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route("/", post(classifying_answer)).with_state((classified.clone(), calls.clone()));
    let source = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = AccessClient::new(&config).unwrap();
    let node = Node::start_with_policy(&dir, Some(config.policy()));
    activate(&node, config.policy()).await;
    let trader = eip712::address(client_key().verifying_key());
    let mut funding = deposit(0, 1, Beneficiary::Trader(trader), 50 * USDC);
    funding.payer = Some(trader);
    assert!(crate::custody::credit::credit(&node.tx, &funding, Some(&client), true).await.is_ok());
    let reference = funding.chain_ref();
    let pending = reference.clone();
    assert!(
        node.read(move |engine| engine.pending_deposits().contains_key(&pending)).await,
        "an undeclared, unclassified deposit waits pending"
    );
    let (mut after, seq) = (None, node.last_seq().await);
    let turn = retry_batch(Some(&client), &node.tx, &mut after).await.unwrap();
    assert!(!turn.failing && !turn.unsettled);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "an undeclared account's deposit is not screened again");
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
    assert_eq!(node.balance(trader).await, Some(engine_core::Fixed::from_int(50)));
    assert!(node.read(|engine| engine.pending_deposits().is_empty() && engine.held_deposits().is_empty()).await);
    node.stop().await;
    source.abort();
    let _ = source.await;
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

fn access_deposits(dir: &std::path::Path) -> usize {
    log_entries(dir)
        .into_iter()
        .filter(|entry| {
            matches!(entry, LogEntry::Command(command)
        if matches!(command.sequenced.cmd, Command::AccessDeposit { .. }))
        })
        .count()
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

/// `screening` signed by `key` under the operator's domain.
fn signed_by(key: &SigningKey, mut screening: Screening) -> Screening {
    let operator = operator();
    screening.signature =
        eip712::sign(key, &eip712::screening_hash(&eip712::domain(operator.chain_id, operator.vault), &screening));
    screening
}
