//! Signs a client request for the node API, as a client holding the account key does.
//!
//! ```text
//! SIGNER_KEY=<private key> cargo run -p engine-core --example sign_request -- \
//!     <chain id> <vault> <nonce> <expires at> \
//!     <order|cancel|cancel_all|close_all|withdrawal> '<body>'
//! ```
//!
//! The account's private key comes from `SIGNER_KEY`, never the command line, where other users
//! could read it. `<body>` is the unsigned JSON body of `POST /v1/orders`, `/v1/orders/cancel`,
//! `/v1/orders/cancel-all`, `/v1/positions/close-all`, or `/v1/withdrawals`; the output is that body
//! with its `auth` added. The nonce is the account's `next_nonce` from
//! `GET /v1/accounts/{address}/nonce`. `<expires at>` is the latest time, in milliseconds since the
//! Unix epoch, the request may execute at; the engine refuses one more than five minutes ahead
//! (`MAX_EXPIRY_AHEAD_MS`). Run on a request from `tests/fixtures/eip712.json`, it prints that
//! request's signature; a wallet signing the request's typed data with `eth_signTypedData_v4` prints
//! the same.

use alloy_primitives::hex;
use anyhow::{Context, anyhow, bail};
use engine_core::access::EligibilityClass;
use engine_core::auth::RequestAuth;
use engine_core::command::{CancelAllRequest, CancelRequest, CloseAllRequest, OrderRequest, WithdrawalRequest};
use engine_core::eip712::{
    self, cancel_all_hash, cancel_hash, close_all_hash, eligibility_hash, order_hash, withdrawal_hash,
};
use engine_core::types::{AccountId, Millis};
use k256::ecdsa::SigningKey;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EligibilityBody {
    account: AccountId,
    class: EligibilityClass,
}

/// Prints the signed body.
/// Error cases:
/// - `SIGNER_KEY` is unset or not a secp256k1 private key
/// - the wrong number of arguments, or one that does not parse
/// - a body that is not a request of the named kind
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [chain_id, vault, nonce, expires_at, kind, body] = args.as_slice() else {
        bail!(
            "usage: SIGNER_KEY=<private key> sign_request <chain id> <vault> <nonce> <expires at> \
             <order|cancel|cancel_all|close_all|withdrawal> <body>"
        );
    };
    let key = std::env::var("SIGNER_KEY").context("SIGNER_KEY")?;
    let key = SigningKey::from_slice(&hex::decode(key).context("SIGNER_KEY")?)
        .map_err(|_| anyhow!("SIGNER_KEY is not a secp256k1 private key"))?;
    let domain = eip712::domain(chain_id.parse().context("chain id")?, vault.parse().context("vault")?);
    let nonce: u64 = nonce.parse().context("nonce")?;
    let expires_at: Millis = expires_at.parse().context("expires at")?;
    let mut body: Value = serde_json::from_str(body).context("body")?;
    let hash = match kind.as_str() {
        "eligibility" => {
            let declaration: EligibilityBody = serde_json::from_value(body.clone())?;
            eligibility_hash(&domain, declaration.account, declaration.class, nonce, expires_at)
        }
        "order" => order_hash(&domain, &serde_json::from_value::<OrderRequest>(body.clone())?, nonce, expires_at),
        "cancel" => {
            let cancel: CancelRequest = serde_json::from_value(body.clone())?;
            cancel_hash(&domain, cancel.account, cancel.order_id, nonce, expires_at)
        }
        "cancel_all" => {
            let cancel: CancelAllRequest = serde_json::from_value(body.clone())?;
            cancel_all_hash(&domain, cancel.account, nonce, expires_at)
        }
        "close_all" => {
            close_all_hash(&domain, &serde_json::from_value::<CloseAllRequest>(body.clone())?, nonce, expires_at)
        }
        "withdrawal" => {
            let withdrawal: WithdrawalRequest = serde_json::from_value(body.clone())?;
            withdrawal_hash(&domain, withdrawal.account, withdrawal.amount, nonce, expires_at)
        }
        other => bail!(
            "unknown request kind {other:?}; expected order, cancel, cancel_all, close_all, withdrawal, or eligibility"
        ),
    };
    body["auth"] = serde_json::to_value(RequestAuth { nonce, expires_at, signature: eip712::sign(&key, &hash) })?;
    println!("{body}");
    Ok(())
}
