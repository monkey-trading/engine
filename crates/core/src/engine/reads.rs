//! The engine's read accessors: its position in the log, its protocol, and its markets and accounts.

use std::collections::BTreeMap;

use crate::account::Account;
use crate::auth::{ClientAuth, Operator};
use crate::engine::{Engine, MarketState};
use crate::params::ProtocolParams;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, Millis};

impl Engine {
    /// Seq of the last applied command, or `None` before genesis.
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Sequenced time of the last applied command.
    pub fn now(&self) -> Millis {
        self.now
    }

    /// The protocol parameters, once set.
    pub fn params(&self) -> Option<&ProtocolParams> {
        self.params.as_ref()
    }

    /// The protocol version in force.
    pub fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    /// How client requests are authorised from `V3`, once recorded.
    pub fn client_auth(&self) -> Option<&ClientAuth> {
        self.client_auth.as_ref()
    }

    /// The nonce `account`'s next signed request must carry; for the operator's address, its next
    /// admin command's.
    pub fn next_request_nonce(&self, account: AccountId) -> u64 {
        self.request_nonces.get(&account).copied().unwrap_or(0)
    }

    /// Who signs admin commands from `V8`, once recorded.
    pub fn operator(&self) -> Option<&Operator> {
        self.operator.as_ref()
    }

    /// Every listed market, by id.
    pub fn markets(&self) -> &BTreeMap<MarketId, MarketState> {
        &self.markets
    }

    /// Every account that has ever been credited, by id.
    pub fn accounts(&self) -> &BTreeMap<AccountId, Account> {
        &self.accounts
    }
}
