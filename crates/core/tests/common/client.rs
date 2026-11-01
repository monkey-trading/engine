//! A client key that signs requests and operator commands under the harness's domain.

use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, AdminRequest, Command, OrderRequest};
use engine_core::eip712::{self, B256, Eip712Domain, admin_hash, cancel_hash, order_hash, withdrawal_hash};
use engine_core::types::{AccountId, Millis, OrderId};
use k256::ecdsa::SigningKey;

use crate::common::{CHAIN_ID, OPERATOR_SEED, VAULT, f, start};

/// A client key, the account it controls, and the domain it signs under.
pub struct Client {
    pub key: SigningKey,
    pub account: AccountId,
    pub domain: Eip712Domain,
    /// The expiry every request this client signs carries.
    pub expires_at: Millis,
}

impl Client {
    /// A distinct key per `seed`, signing under the signed harness's domain, with requests that
    /// expire a minute after the harness starts.
    pub fn new(seed: u8) -> Self {
        let key = SigningKey::from_slice(&[seed; 32]).expect("a valid scalar");
        let account = eip712::address(key.verifying_key());
        Client { key, account, domain: eip712::domain(CHAIN_ID, VAULT), expires_at: start() + 60_000 }
    }

    /// `req` with this key's signature over it and `nonce`, whatever account it names.
    pub fn order(&self, nonce: u64, mut req: OrderRequest) -> Command {
        let hash = order_hash(&self.domain, &req, nonce, self.expires_at);
        req.auth = Some(self.auth(nonce, hash));
        Command::PlaceOrder(req)
    }

    /// The operator key the `V8` harness records.
    pub fn operator() -> Self {
        Client::new(OPERATOR_SEED)
    }

    /// `command` with this key's signature over it and `nonce`, as the operator sends it.
    pub fn admin(&self, nonce: u64, command: AdminCommand) -> Command {
        let hash = admin_hash(&self.domain, &command, nonce, self.expires_at);
        Command::SignedAdmin(Box::new(AdminRequest { command, auth: self.auth(nonce, hash) }))
    }

    /// A signed cancel of `order_id` for this account.
    pub fn cancel(&self, nonce: u64, order_id: OrderId) -> Command {
        let hash = cancel_hash(&self.domain, self.account, order_id, nonce, self.expires_at);
        Command::CancelOrder { account: self.account, order_id, auth: Some(self.auth(nonce, hash)) }
    }

    /// A signed withdrawal of `amount` for this account.
    pub fn withdraw(&self, nonce: u64, amount: &str) -> Command {
        let hash = withdrawal_hash(&self.domain, self.account, f(amount), nonce, self.expires_at);
        Command::Withdraw { account: self.account, amount: f(amount), auth: Some(self.auth(nonce, hash)) }
    }

    /// A signed registration of `session_key` for this account until `session_expires_at`.
    pub fn register_session_key(&self, nonce: u64, session_key: AccountId, session_expires_at: Millis) -> Command {
        let hash = eip712::session_key_registration_hash(
            &self.domain,
            self.account,
            session_key,
            session_expires_at,
            nonce,
            self.expires_at,
        );
        Command::RegisterSessionKey {
            account: self.account,
            session_key,
            expires_at: session_expires_at,
            auth: Some(self.auth(nonce, hash)),
        }
    }

    /// A signed revocation of `session_key` for this account.
    pub fn revoke_session_key(&self, nonce: u64, session_key: AccountId) -> Command {
        let hash = eip712::session_key_revocation_hash(&self.domain, self.account, session_key, nonce, self.expires_at);
        Command::RevokeSessionKey { account: self.account, session_key, auth: Some(self.auth(nonce, hash)) }
    }

    /// This key's signature over `hash`, with `nonce` and this client's expiry.
    fn auth(&self, nonce: u64, hash: B256) -> RequestAuth {
        RequestAuth { nonce, expires_at: self.expires_at, signature: eip712::sign(&self.key, &hash) }
    }
}
