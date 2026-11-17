//! The signed order and withdrawal bodies the access tests post.

use engine_core::Fixed;
use engine_core::auth::RequestAuth;
use engine_core::command::{OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::eip712;
use engine_core::types::Side;
use k256::ecdsa::SigningKey;
use serde_json::{Value, json};

use super::{NOW, VAULT};

/// A signed market buy of 0.01 in market 1 by `key`'s account at `nonce`.
pub(super) fn order(key: &SigningKey, nonce: u64) -> Value {
    let account = eip712::address(key.verifying_key());
    let mut request = OrderRequest {
        account,
        client_id: 1,
        market: 1,
        side: Side::Buy,
        qty: "0.01".parse().unwrap(),
        kind: OrderKind::Market { quoted: Fixed::from_int(100000), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
        take_profit: None,
        stop_loss: None,
    };
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, nonce, NOW + 60_000);
    request.auth = Some(RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(key, &hash) });
    json!(request)
}

/// A signed withdrawal of 10 USDC by `key`'s account at `nonce`.
pub(super) fn withdrawal(key: &SigningKey, nonce: u64) -> Value {
    let account = eip712::address(key.verifying_key());
    let amount = Fixed::from_int(10);
    let hash = eip712::withdrawal_hash(&eip712::domain(31_337, VAULT), account, amount, nonce, NOW + 60_000);
    json!(WithdrawalRequest {
        account,
        amount,
        auth: Some(RequestAuth { nonce, expires_at: NOW + 60_000, signature: eip712::sign(key, &hash) })
    })
}
