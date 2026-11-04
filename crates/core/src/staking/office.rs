//! The operator's share split by time in office, from `V50`.
//!
//! While rotation is on, each command first credits the sequenced time since the command before it
//! to the account that held the office over it, as that command left the state: the candidate seated
//! under the key in effect, or the key's address for an operator no election installed
//! (`Engine::operator_share_account`). A handover therefore counts from the command that applies it,
//! including a close's own command, whose time before it is the outgoing operator's. The open epoch
//! counts from the command that closed the epoch before it, or from `V50`'s activation or staking's
//! initialisation under it, to the command that closes it, skipped epochs included, as that close
//! pays the excess they built up. The close splits `floor(excess / 5)` by that time: each account
//! earns `floor(share · served / counted)`, where `counted` is all the time the open epoch counted,
//! so the time rotation was off, which no one served, and the rounding go to stakers.

use std::collections::BTreeMap;

use alloy_primitives::U256;
use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, Millis};

/// From `V50`, when the open distribution epoch began counting time in office, and each account's
/// time in office since.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeInOffice {
    /// When the open epoch began counting, in sequenced milliseconds.
    pub since: Millis,
    /// Each account's sequenced time in office since `since`, in milliseconds; never zero.
    pub served: BTreeMap<AccountId, Millis>,
}

impl Engine {
    /// From `V50`, credits the sequenced time from `from` to now to the account in office over it,
    /// before this command's handover can move the office.
    pub(crate) fn accrue_time_in_office(&mut self, from: Millis) {
        if self.protocol_version < ProtocolVersion::V50 {
            return;
        }
        let (Some(holder), Some(staking)) = (self.operator_share_account(), self.staking.as_mut()) else { return };
        let office = staking.office.as_mut().expect("V50 counts time in office once staking is initialised");
        let served = self.now - from;
        if served > 0 {
            *office.served.entry(holder).or_default() += served;
        }
    }

    /// From `V50`, `share` USDC base units split by the open epoch's time in office, in account order:
    /// `floor(share · served / counted)` each, leaving out the parts that round to nothing.
    pub(crate) fn split_by_time_in_office(&self, share: u128) -> Vec<(AccountId, u128)> {
        let office = self.staking.as_ref().and_then(|staking| staking.office.as_ref());
        let office = office.expect("V50 counts time in office once staking is initialised");
        // Positive whenever anyone served: every credit lies between `since` and now.
        let counted = U256::from((self.now - office.since) as u128);
        office
            .served
            .iter()
            .filter_map(|(account, served)| {
                // At most `share`, as no account served longer than the epoch counted.
                let amount: u128 = (U256::from(share) * U256::from(*served as u128) / counted).to();
                (amount > 0).then_some((*account, amount))
            })
            .collect()
    }

    /// Starts counting time in office now, afresh: at `V50`'s activation, at staking's initialisation
    /// under it, and at each close.
    pub(crate) fn start_time_in_office(&mut self) {
        let since = self.now;
        if let Some(staking) = self.staking.as_mut() {
            staking.office = Some(TimeInOffice { since, served: BTreeMap::new() });
        }
    }
}
