//! The published signing vectors (`crates/core/tests/fixtures/eip712.json`) as bodies the API takes.

use engine_core::command::{
    CancelAllRequest, CancelRequest, CloseAllRequest, OrderRequest, PositionLevelsRequest, WithdrawalRequest,
};
use serde_json::Value;

const VECTORS: &str = include_str!("../../../core/tests/fixtures/eip712.json");

#[test]
fn the_published_signing_vectors_are_bodies_the_api_accepts() {
    let vectors: Value = serde_json::from_str(VECTORS).unwrap();
    for vector in vectors["requests"].as_array().unwrap() {
        let mut body = vector["body"].clone();
        let (nonce, expires_at, signature) = (&vector["nonce"], &vector["expires_at"], &vector["signature"]);
        body["auth"] = serde_json::json!({ "nonce": nonce, "expires_at": expires_at, "signature": signature });
        let parsed = match vector["kind"].as_str().unwrap() {
            "order" => serde_json::from_value::<OrderRequest>(body).map(drop),
            "cancel" => serde_json::from_value::<CancelRequest>(body).map(drop),
            "withdrawal" => serde_json::from_value::<WithdrawalRequest>(body).map(drop),
            "cancel_all" => serde_json::from_value::<CancelAllRequest>(body).map(drop),
            "close_all" => serde_json::from_value::<CloseAllRequest>(body).map(drop),
            "position_levels" => serde_json::from_value::<PositionLevelsRequest>(body).map(drop),
            "register_session_key" => serde_json::from_value::<crate::api::SessionKeyRequest>(body).map(drop),
            "revoke_session_key" => serde_json::from_value::<crate::api::SessionKeyRevocationRequest>(body).map(drop),
            other => panic!("unknown request kind {other}"),
        };
        assert!(parsed.is_ok(), "{}: {parsed:?}", vector["kind"]);
    }
}
