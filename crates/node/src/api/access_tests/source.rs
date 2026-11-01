//! The mock screening source the access tests answer with.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use engine_core::access::{Jurisdiction, SanctionsStatus, Screening};
use engine_core::eip712::{self, Signature};
use engine_core::types::AccountId;
use serde_json::Value;

use super::{NOW, VAULT, key};

pub(super) struct Source {
    pub(super) jurisdiction: Jurisdiction,
    pub(super) sanctioned: BTreeSet<AccountId>,
    pub(super) forged: bool,
    pub(super) requests: Vec<Value>,
}

pub(super) async fn answer(State(source): State<Arc<Mutex<Source>>>, Json(request): Json<Value>) -> Json<Screening> {
    let mut source = source.lock().unwrap();
    source.requests.push(request.clone());
    let account: Option<AccountId> = serde_json::from_value(request["account"].clone()).unwrap();
    let payer: Option<AccountId> = serde_json::from_value(request["payer"].clone()).unwrap();
    let status = |address: Option<AccountId>| {
        if address.is_some_and(|address| source.sanctioned.contains(&address)) {
            SanctionsStatus::Sanctioned
        } else {
            SanctionsStatus::Clear
        }
    };
    let mut screening = Screening {
        source: eip712::address(key(0x77).verifying_key()),
        request_hash: serde_json::from_value(request["request_hash"].clone()).unwrap(),
        observed_at: NOW,
        account,
        payer,
        jurisdiction: source.jurisdiction,
        account_status: status(account),
        payer_status: status(payer),
        signature: Signature::ZERO,
    };
    if !source.forged {
        screening.signature =
            eip712::sign(&key(0x77), &eip712::screening_hash(&eip712::domain(31_337, VAULT), &screening));
    }
    Json(screening)
}
