//! Signed source for the real-vault checkpoint scenario.

use crate::log::CheckpointDomain;
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
