//! From `V88`, the elections waiting behind the last: each takes effect in turn at its own time, as the
//! registry seated it on chain before it elected the next, and the pending handover is always the first
//! one's. A command after several of their times takes the latest one's key at once, before any turn
//! hands over, as a single pending rotation would.

use crate::election::{Election, SeatedCandidate};
use crate::engine::Engine;
use crate::operator_bounds::PendingRotation;
use crate::types::{AccountId, Millis};

impl Engine {
    /// The key due before a command stamped `ts` once the first waiting election's time has come: the
    /// latest election in turn, the last included, whose time has come by `ts` and whose registry time
    /// the staking mirror's finalised frontier has reached, or `Some(None)` when that key is the one in
    /// effect. `None` while no waiting election's time has come, so `due_rotation` reads the pending
    /// rotation as before `V88`.
    pub(super) fn due_waiting_key(&self, ts: Millis) -> Option<Option<PendingRotation>> {
        let due = |election: &&Election| self.taken_effect_by(election, ts);
        self.operator_election.waiting.first().filter(due)?;
        let elections = self.operator_election.waiting.iter().chain(self.operator_election.last.iter());
        let latest = elections.filter(|election| !election.cancelled).take_while(due).last()?;
        let operator = self.operator.map(|operator| operator.address);
        Some(
            (operator != Some(latest.admin_key))
                .then_some(PendingRotation { address: latest.admin_key, effective_at: latest.effective_at }),
        )
    }

    /// Seats each waiting election whose time has come by `ts`, in turn, once `apply_due_rotation` has
    /// moved the key (`due_waiting_key`), and leaves the next one's handover pending: the next waiting
    /// election's, else the last's, unless its key is the one in effect.
    pub(crate) fn advance_waiting_elections(&mut self, ts: Millis) {
        let waiting = &self.operator_election.waiting;
        let taken = waiting.iter().take_while(|election| self.taken_effect_by(election, ts)).count();
        let Some(seated) = taken.checked_sub(1).and_then(|latest| waiting.get(latest).copied()) else { return };
        self.operator_election.waiting = self.operator_election.waiting.split_off(taken);
        self.operator_election.seated =
            Some(SeatedCandidate { candidate: seated.candidate, admin_key: seated.admin_key });
        let operator = self.operator.expect("an election is recorded beside an operator").address;
        let next = self.operator_election.waiting.first().copied().or(self.operator_election.last);
        self.operator_bounds.rotation = next
            .filter(|next| !next.cancelled && next.admin_key != operator)
            .map(|next| PendingRotation { address: next.admin_key, effective_at: next.effective_at });
    }

    /// Whether an election governs the operator key: one handing it over, as the pending rotation is
    /// its, or, not cancelled, one whose key is the one in effect. From `V88` a waiting election
    /// counts as the last does.
    pub fn election_governs_key(&self) -> bool {
        let operator = self.operator.map(|operator| operator.address);
        let elections = self.operator_election.waiting.iter().chain(self.operator_election.last.iter());
        self.operator_bounds.rotation.is_some_and(|rotation| self.is_elected(rotation))
            || elections.filter(|election| !election.cancelled).any(|election| Some(election.admin_key) == operator)
    }

    /// The keys of the elections still to take effect while any waits (`V88`): each waiting election's and
    /// the last's behind them, not cancelled. None while none waits, as the pending rotation then names the
    /// next key.
    pub fn awaited_election_keys(&self) -> impl Iterator<Item = AccountId> + '_ {
        let waiting = &self.operator_election.waiting;
        let behind = self.operator_election.last.filter(|_| !waiting.is_empty());
        waiting.iter().copied().chain(behind).filter(|election| !election.cancelled).map(|election| election.admin_key)
    }

    /// Whether `election`'s time has come by `ts` and the staking mirror's finalised frontier has
    /// reached its registry time, as `due_rotation` requires of an elected key.
    fn taken_effect_by(&self, election: &Election, ts: Millis) -> bool {
        election.effective_at <= ts && self.finalised_chain_time() >= Some(election.registry_effective_at)
    }
}
