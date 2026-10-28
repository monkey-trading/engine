//! Signed source for the real-vault checkpoint scenario.

use engine_core::Fixed;
use tokio::sync::{mpsc, oneshot};

use crate::log::CheckpointDomain;
use crate::sequencer::Request;
use crate::test_support::access;
use std::sync::Arc;

/// Signed, clear synthetic classifications; the scenario's real vault domain is mandatory.
pub(super) async fn start(domain: CheckpointDomain) -> (Arc<crate::access::AccessClient>, tokio::task::JoinHandle<()>) {
    use axum::{Json, Router, routing::post};
    use engine_core::access::{Jurisdiction, SanctionsStatus, Screening};
    use engine_core::eip712::{self, Signature};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cfg: crate::config::AccessSection = toml::from_str(&format!(
        "source='{}'\nendpoint='http://{}'\ntimeout='2s'\nmax_age='60s'\nmax_response_bytes=4096\nmax_in_flight=2",
        access::policy().source,
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let app = Router::new().route(
        "/",
        post(move |Json(request): Json<serde_json::Value>| async move {
            let mut screening = Screening {
                source: access::policy().source,
                request_hash: serde_json::from_value(request["request_hash"].clone()).unwrap(),
                observed_at: crate::clock::now_ms(),
                account: serde_json::from_value(request["account"].clone()).unwrap(),
                payer: serde_json::from_value(request["payer"].clone()).unwrap(),
                jurisdiction: Jurisdiction::Allowed,
                account_status: SanctionsStatus::Clear,
                payer_status: SanctionsStatus::Clear,
                signature: Signature::ZERO,
            };
            screening.signature = eip712::sign(
                &access::key(0x77),
                &eip712::screening_hash(&eip712::domain(domain.chain_id, domain.vault), &screening),
            );
            Json(screening)
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (Arc::new(crate::access::AccessClient::new(&cfg).unwrap()), task)
}

/// Polls the sequencer until exactly `count` deposits wait pending, as the deposit adapter's
/// screening of an undeclared beneficiary leaves them.
pub(super) async fn await_pending(tx: &tokio::sync::mpsc::Sender<crate::sequencer::Request>, count: usize) {
    crate::test_support::wait_for("the pending deposit count the screening leaves", async {
        loop {
            let (reply, response) = tokio::sync::oneshot::channel();
            tx.send(crate::sequencer::Request::Query(Box::new(move |engine| {
                let _ = reply.send(engine.pending_deposits().len());
            })))
            .await
            .unwrap();
            if response.await.unwrap() == count {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await;
}

/// The held deposits and their reasons, in reference order.
pub(super) async fn held(
    tx: &tokio::sync::mpsc::Sender<crate::sequencer::Request>,
) -> Vec<(String, engine_core::access::AccessReject)> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(crate::sequencer::Request::Query(Box::new(move |engine| {
        let _ = reply
            .send(engine.held_deposits().iter().map(|(reference, held)| (reference.clone(), held.reason)).collect());
    })))
    .await
    .unwrap();
    crate::test_support::wait_for("the sequencer to answer a query", rx).await.unwrap()
}

/// The undeclared account's pending deposit and the credit reconcile to both real vault deposits.
pub(super) fn assert_pending_reconciliation(replayed: &crate::sequencer::Replayed) {
    use engine_core::Fixed;
    assert_eq!(replayed.engine.house_cash(), Fixed::from_int(100));
    assert!(!replayed.engine.accounts().contains_key(&access::account(1)));
    let pending = replayed.engine.pending_deposits().values().next().unwrap();
    assert_eq!((pending.account, pending.amount), (Some(access::account(1)), Fixed::from_int(10)));
    assert!(replayed.engine.held_deposits().is_empty());
    assert_eq!(
        (replayed.accounting.pending, replayed.accounting.liabilities, replayed.accounting.deposits),
        (Fixed::from_int(10), Fixed::from_int(110), 2)
    );
}

/// The scenario engine's house cash.
pub(super) async fn house(tx: &mpsc::Sender<Request>) -> Fixed {
    let (reply, rx) = oneshot::channel();
    tx.send(Request::Query(Box::new(move |engine| {
        let _ = reply.send(engine.house_cash());
    })))
    .await
    .unwrap();
    crate::test_support::wait_for("the sequencer to answer a query", rx).await.unwrap()
}
