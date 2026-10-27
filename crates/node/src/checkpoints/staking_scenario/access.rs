//! Signed loopback screening and the published Anvil client keys for this scenario.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use engine_core::access::{AccessPolicy, Jurisdiction, SanctionsStatus, Screening};
use engine_core::auth::Operator;
use engine_core::eip712::{self, Signature};
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::access::AccessClient;
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::config::AccessSection;

/// A bounded production client backed by a signed, deterministic loopback source.
pub(super) struct Source {
    pub(super) client: Arc<AccessClient>,
    pub(super) policy: AccessPolicy,
    task: JoinHandle<()>,
}

impl Source {
    /// Starts the source under the deployed vault's domain and the scenario's sequenced clock.
    pub(super) async fn start(clock: Arc<TestClock>, operator: Operator) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let key = SigningKey::from_slice(&[0x77; 32]).unwrap();
        let config: AccessSection = toml::from_str(&format!(
            "source='{}'\nendpoint='http://{}'\ntimeout='2s'\nmax_age='60s'\nmax_response_bytes=4096\nmax_in_flight=4",
            eip712::address(key.verifying_key()),
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let app = Router::new().route("/", post(answer)).with_state((clock, operator));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { client: Arc::new(AccessClient::new(&config).unwrap()), policy: config.policy(), task }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Classifies the fixture's accounts and transport peers as clear, with freshly signed evidence.
async fn answer(
    State((clock, operator)): State<(Arc<TestClock>, Operator)>,
    Json(request): Json<Value>,
) -> Json<Screening> {
    let key = SigningKey::from_slice(&[0x77; 32]).unwrap();
    let mut proof = Screening {
        source: eip712::address(key.verifying_key()),
        request_hash: serde_json::from_value(request["request_hash"].clone()).unwrap(),
        observed_at: clock.now_ms(),
        account: serde_json::from_value(request["account"].clone()).unwrap(),
        payer: serde_json::from_value(request["payer"].clone()).unwrap(),
        jurisdiction: Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: SanctionsStatus::Clear,
        signature: Signature::ZERO,
    };
    proof.signature =
        eip712::sign(&key, &eip712::screening_hash(&eip712::domain(operator.chain_id, operator.vault), &proof));
    Json(proof)
}

/// The private key of one of the two published default Anvil accounts the scenario stakes with.
pub(super) fn client_key(account: AccountId) -> SigningKey {
    for encoded in [
        "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
        "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
    ] {
        let key = SigningKey::from_slice(&hex::decode(encoded).unwrap()).unwrap();
        if eip712::address(key.verifying_key()) == account {
            return key;
        }
    }
    panic!("the scenario account has no published Anvil signing key");
}
