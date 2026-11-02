//! EIP-712 typed-data signatures: the domain, the request types clients and the operator sign, and
//! signer recovery.
//!
//! A client signs each order, cancel, and withdrawal with its account key, as a wallet does with
//! `eth_signTypedData_v4`. The domain names the custody chain and vault, and each request carries
//! the account's next nonce and an expiry, so one signature authorises one request on one
//! deployment, accepted within a few minutes at most; an order it places then rests as usual. The
//! expiry is checked against sequenced time, which follows the host clock (see
//! `MAX_EXPIRY_AHEAD_MS`). Amounts and prices are `int128` in `Fixed`'s raw units (9 decimals);
//! `expiresAt` is milliseconds since the Unix epoch, the unit of sequenced time; sides and order
//! kinds are their JSON names. `crates/core/tests/fixtures/eip712.json` holds the
//! typed data and signatures of each client request type, and `examples/sign_request.rs` signs a request
//! body for the API. From protocol `V8` the operator signs admin commands under the same domain,
//! each as its JSON, with the operator's next nonce and an expiry; `engine admin` signs them.

pub use alloy_primitives::B256;
use alloy_primitives::{Address, FixedBytes, U256};
pub use alloy_sol_types::Eip712Domain;
use alloy_sol_types::SolStruct;
use k256::ecdsa::signature::hazmat::PrehashVerifier;
use k256::ecdsa::{RecoveryId, Signature as EcdsaSignature, SigningKey, VerifyingKey};
use serde::Serialize;
use sha3::{Digest, Keccak256};

use crate::Fixed;
use crate::access::{AccessDeposit, EligibilityClass, Screening};
use crate::command::{AdminCommand, CloseAllRequest, CloseEntry, OrderKind, OrderRequest};
use crate::types::{AccountId, Millis, OrderId, Side};

/// The domain name every request is signed under.
pub const DOMAIN_NAME: &str = "monkeytrading";
/// The version of the request types below, which `V3` signs under. A protocol version that
/// changes a request type adds new types under a new domain version and keeps these.
pub const DOMAIN_VERSION: &str = "1";

/// A 65-byte `r || s || v` secp256k1 signature with `v` in {27, 28}, as wallets return it.
pub type Signature = FixedBytes<65>;

/// The structs clients sign, named apart from the engine's own types.
mod typed {
    use alloy_sol_types::sol;

    sol! {
        /// `Command::PlaceOrder`. `price` is a market order's quoted price, or a limit or stop
        /// order's trigger price. `hasMaxSlippage` is false when the order takes the market's bound.
        struct OrderRequest {
            address account;
            uint64 nonce;
            int64 expiresAt;
            uint64 clientId;
            uint32 market;
            string side;
            int128 qty;
            string kind;
            int128 price;
            bool hasMaxSlippage;
            int64 maxSlippageBps;
            bool reduceOnly;
        }

        /// `Command::CancelOrder`.
        struct CancelRequest {
            address account;
            uint64 nonce;
            int64 expiresAt;
            uint64 orderId;
        }

        /// `Command::CancelAll`.
        struct CancelAllRequest {
            address account;
            uint64 nonce;
            int64 expiresAt;
        }

        /// One entry of `Command::AccessCloseAll`: the market and the price the client saw.
        struct CloseEntry {
            uint32 market;
            int128 quoted;
        }

        /// `Command::AccessCloseAll`. The list is signed in order; `hasMaxSlippage` is false when the
        /// closes take each market's own bound.
        struct CloseAllRequest {
            address account;
            uint64 nonce;
            int64 expiresAt;
            CloseEntry[] closes;
            bool hasMaxSlippage;
            int64 maxSlippageBps;
        }

        /// `Command::Withdraw`.
        struct WithdrawalRequest {
            address account;
            uint64 nonce;
            int64 expiresAt;
            int128 amount;
        }

        /// `Command::SignedAdmin`. `command` is the admin command's JSON as the engine encodes it,
        /// so a wallet shows the operator exactly what it signs.
        struct AdminRequest {
            uint64 nonce;
            int64 expiresAt;
            string command;
        }

        /// `Command::RegisterSessionKey`. `sessionExpiresAt` caps how long the key may trade;
        /// `expiresAt` bounds this request as any signed request's does.
        struct RegisterSessionKey {
            address account;
            uint64 nonce;
            int64 expiresAt;
            address sessionKey;
            int64 sessionExpiresAt;
        }

        /// `Command::RevokeSessionKey`.
        struct RevokeSessionKey {
            address account;
            uint64 nonce;
            int64 expiresAt;
            address sessionKey;
        }

        /// The statement names only the eligibility class the client declares.
        struct EligibilityRequest {
            address account;
            uint64 nonce;
            int64 expiresAt;
            string statement;
        }

        /// The configured screening authority signs its canonical public decision.
        struct AccessScreening {
            string decision;
        }
    }
}

/// The domain of the vault `vault` on chain `chain_id`.
pub fn domain(chain_id: u64, vault: AccountId) -> Eip712Domain {
    Eip712Domain::new(
        Some(DOMAIN_NAME.into()),
        Some(DOMAIN_VERSION.into()),
        Some(U256::from(chain_id)),
        Some(Address::from(vault.0)),
        None,
    )
}

/// The digest a client signs to place `req` with `nonce`, until `expires_at`.
pub fn order_hash(domain: &Eip712Domain, req: &OrderRequest, nonce: u64, expires_at: Millis) -> B256 {
    // Every field is named, so a field added to `OrderRequest` fails to compile until it is signed.
    let OrderRequest { account, client_id, market, side, qty, kind, reduce_only, auth: _ } = req;
    let (kind, price, max_slippage_bps) = match *kind {
        OrderKind::Market { quoted, max_slippage_bps } => ("market", quoted, max_slippage_bps),
        OrderKind::Limit { price } => ("limit", price, None),
        OrderKind::Stop { price } => ("stop", price, None),
    };
    let side = match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    };
    typed::OrderRequest {
        account: Address::from(account.0),
        nonce,
        expiresAt: expires_at,
        clientId: *client_id,
        market: *market,
        side: side.into(),
        qty: qty.raw(),
        kind: kind.into(),
        price: price.raw(),
        hasMaxSlippage: max_slippage_bps.is_some(),
        maxSlippageBps: max_slippage_bps.unwrap_or(0),
        reduceOnly: *reduce_only,
    }
    .eip712_signing_hash(domain)
}

/// The digest a client signs to cancel `order_id` with `nonce`, until `expires_at`.
pub fn cancel_hash(
    domain: &Eip712Domain,
    account: AccountId,
    order_id: OrderId,
    nonce: u64,
    expires_at: Millis,
) -> B256 {
    typed::CancelRequest { account: Address::from(account.0), nonce, expiresAt: expires_at, orderId: order_id }
        .eip712_signing_hash(domain)
}

/// The digest a client signs to cancel all its resting orders with `nonce`, until `expires_at`.
pub fn cancel_all_hash(domain: &Eip712Domain, account: AccountId, nonce: u64, expires_at: Millis) -> B256 {
    typed::CancelAllRequest { account: Address::from(account.0), nonce, expiresAt: expires_at }
        .eip712_signing_hash(domain)
}

/// The digest a client signs to close the positions `req` names with `nonce`, until `expires_at`.
pub fn close_all_hash(domain: &Eip712Domain, req: &CloseAllRequest, nonce: u64, expires_at: Millis) -> B256 {
    // Every field is named, so a field added to `CloseAllRequest` fails to compile until it is signed.
    let CloseAllRequest { account, closes, max_slippage_bps, auth: _ } = req;
    typed::CloseAllRequest {
        account: Address::from(account.0),
        nonce,
        expiresAt: expires_at,
        closes: closes
            .iter()
            .map(|CloseEntry { market, quoted }| typed::CloseEntry { market: *market, quoted: quoted.raw() })
            .collect(),
        hasMaxSlippage: max_slippage_bps.is_some(),
        maxSlippageBps: max_slippage_bps.unwrap_or(0),
    }
    .eip712_signing_hash(domain)
}

/// The digest a client signs to withdraw `amount` with `nonce`, until `expires_at`.
pub fn withdrawal_hash(
    domain: &Eip712Domain,
    account: AccountId,
    amount: Fixed,
    nonce: u64,
    expires_at: Millis,
) -> B256 {
    typed::WithdrawalRequest { account: Address::from(account.0), nonce, expiresAt: expires_at, amount: amount.raw() }
        .eip712_signing_hash(domain)
}

/// The digest the operator signs to apply `command` with `nonce`, until `expires_at`.
pub fn admin_hash(domain: &Eip712Domain, command: &AdminCommand, nonce: u64, expires_at: Millis) -> B256 {
    // The engine's own encoding is the canonical form: the signer and every replay encode the same
    // command with this code. Every field is a string, integer, list, or map, all of which
    // serde_json encodes.
    let command = serde_json::to_string(command).expect("admin commands serialise");
    typed::AdminRequest { nonce, expiresAt: expires_at, command }.eip712_signing_hash(domain)
}

/// The digest an account signs to register `session_key` for itself until `session_expires_at`,
/// with `nonce`, until `expires_at`.
pub fn session_key_registration_hash(
    domain: &Eip712Domain,
    account: AccountId,
    session_key: AccountId,
    session_expires_at: Millis,
    nonce: u64,
    expires_at: Millis,
) -> B256 {
    typed::RegisterSessionKey {
        account: Address::from(account.0),
        nonce,
        expiresAt: expires_at,
        sessionKey: Address::from(session_key.0),
        sessionExpiresAt: session_expires_at,
    }
    .eip712_signing_hash(domain)
}

/// The digest an account signs to revoke `session_key` for itself with `nonce`, until `expires_at`.
pub fn session_key_revocation_hash(
    domain: &Eip712Domain,
    account: AccountId,
    session_key: AccountId,
    nonce: u64,
    expires_at: Millis,
) -> B256 {
    typed::RevokeSessionKey {
        account: Address::from(account.0),
        nonce,
        expiresAt: expires_at,
        sessionKey: Address::from(session_key.0),
    }
    .eip712_signing_hash(domain)
}

/// The digest of the exact eligibility statement, bound to deployment, account, nonce and expiry.
pub fn eligibility_hash(
    domain: &Eip712Domain,
    account: AccountId,
    class: EligibilityClass,
    nonce: u64,
    expires_at: Millis,
) -> B256 {
    typed::EligibilityRequest {
        account: Address::from(account.0),
        nonce,
        expiresAt: expires_at,
        statement: class.statement().into(),
    }
    .eip712_signing_hash(domain)
}

/// Binds deposit screening to the complete canonical custody input.
pub fn deposit_screening_hash(deposit: &AccessDeposit) -> B256 {
    let bytes = serde_json::to_vec(deposit).expect("deposit inputs serialise");
    B256::from_slice(&Keccak256::digest(bytes))
}

/// The source's signed decision, bound to the same recorded deployment domain as client requests.
pub fn screening_hash(domain: &Eip712Domain, screening: &Screening) -> B256 {
    #[derive(Serialize)]
    struct Decision<'a> {
        source: AccountId,
        request_hash: B256,
        observed_at: Millis,
        account: Option<AccountId>,
        payer: Option<AccountId>,
        jurisdiction: &'a crate::access::Jurisdiction,
        account_status: &'a crate::access::SanctionsStatus,
        payer_status: &'a crate::access::SanctionsStatus,
    }
    let Screening {
        source,
        request_hash,
        observed_at,
        account,
        payer,
        jurisdiction,
        account_status,
        payer_status,
        signature: _,
    } = screening;
    let decision = Decision {
        source: *source,
        request_hash: *request_hash,
        observed_at: *observed_at,
        account: *account,
        payer: *payer,
        jurisdiction,
        account_status,
        payer_status,
    };
    let decision = serde_json::to_string(&decision).expect("screening decisions serialise");
    typed::AccessScreening { decision }.eip712_signing_hash(domain)
}

/// The account whose key signed `hash`, or `None` when `signature` is malformed or recovers no key.
pub fn recover(hash: &B256, signature: &Signature) -> Option<AccountId> {
    let recovery_id = match signature[64] {
        v @ (27 | 28) => RecoveryId::from_byte(v - 27)?,
        _ => return None,
    };
    let ecdsa = EcdsaSignature::from_slice(&signature[..64]).ok()?;
    let key = VerifyingKey::recover_from_prehash(hash.as_slice(), &ecdsa, recovery_id).ok()?;
    key.verify_prehash(hash.as_slice(), &ecdsa).ok()?;
    Some(address(&key))
}

/// The EVM address of `key`: the last 20 bytes of keccak-256 over the uncompressed point.
pub fn address(key: &VerifyingKey) -> AccountId {
    let point = key.to_sec1_point(false);
    let hash = Keccak256::digest(&point.as_bytes()[1..]);
    AccountId(hash[12..].try_into().expect("keccak-256 has 32 bytes"))
}

/// Signs `hash` with `key` in the form `recover` reads. For clients, tests, and the node's enclave
/// key; the engine only verifies.
pub fn sign(key: &SigningKey, hash: &B256) -> Signature {
    let (ecdsa, recovery_id) = key.sign_prehash_recoverable(hash.as_slice());
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&ecdsa.to_bytes());
    out[64] = 27 + recovery_id.to_byte();
    Signature::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> SigningKey {
        SigningKey::from_slice(&[byte; 32]).unwrap()
    }

    #[test]
    fn a_signature_recovers_to_its_signer_only_for_the_hash_it_signed() {
        let (key, hash) = (key(7), B256::repeat_byte(1));
        let signature = sign(&key, &hash);
        assert_eq!(recover(&hash, &signature), Some(address(key.verifying_key())));
        assert_ne!(recover(&B256::repeat_byte(2), &signature), Some(address(key.verifying_key())));
    }

    #[test]
    fn a_malformed_signature_recovers_nobody() {
        let hash = B256::repeat_byte(1);
        let mut signature = sign(&key(7), &hash);
        signature[64] = 1;
        assert_eq!(recover(&hash, &signature), None);
        assert_eq!(recover(&hash, &Signature::ZERO), None);
    }

    #[test]
    fn the_domain_binds_the_chain_and_vault() {
        let vault = AccountId([9; 20]);
        let base = domain(1, vault).separator();
        assert_ne!(base, domain(2, vault).separator());
        assert_ne!(base, domain(1, AccountId([8; 20])).separator());
    }
}
