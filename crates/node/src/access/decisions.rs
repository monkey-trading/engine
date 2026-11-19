//! The API's screening decisions: at most one per account and client address, reused for each of
//! the pair's requests while it is fresh, so a request from a pair with a fresh decision waits on no
//! source call. A pair that sent a request within the last freshness bound is refreshed before its
//! decision runs out (`due`): late enough to spare the source calls, no sooner than half the
//! decision's attachable life after it arrived, and early enough for two attempts, each the source's
//! timeout and a turn of the refresh loop, before it stops being attached; a failed attempt is tried
//! again on the next turn while the decision lasts. A pair with no fresh decision waits for the source.
//!
//! A decision is kept only once it verifies as the engine will check it: signed by the recorded source
//! under the log's domain, about the account with no payer and naming no request, and stamped no
//! further ahead than the engine allows. It is attached only while at least a tenth of the bound is
//! left, the time a request may wait for its stamp, so the engine's check at the sequenced time finds
//! it fresh. A refused decision is kept and reused like a clear one. A newer decision replaces an
//! older one, never the reverse. The pairs are bounded: past `CAPACITY` a new pair is screened per
//! request, as it is with no decision kept, while the pairs kept are reused and refreshed.
//!
//! Each request reads the decisions on its own task, so they sit behind a lock held only to read or
//! write an entry, never across an await, as the screening slots do.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use engine_core::access::{MAX_EVIDENCE_LEAD_MS, Screening};
use engine_core::eip712::{self, Eip712Domain};
use engine_core::types::{AccountId, Millis};
use tracing::debug;

use crate::config::AccessSection;

/// The most pairs kept: far more than send requests within a bound at launch flow.
const CAPACITY: usize = 50_000;

/// An account and the client address its decision was screened for.
pub(crate) type Pair = (AccountId, IpAddr);

/// The decisions kept, with the source and bound they are checked against.
pub(crate) struct Decisions {
    source: AccountId,
    max_age: Millis,
    /// How long a refresh may take: the source's timeout and a turn of the refresh loop.
    lead: Millis,
    /// A turn of the refresh loop.
    turn: Millis,
    capacity: usize,
    /// The most refreshes in flight at once: half the API's screening slots outside the reserve, at
    /// least one, so requests with no fresh decision always find the rest.
    pub(crate) refresh_limit: usize,
    /// Whether the API has seen the log at `V85` or later, from which a decision covers an account and
    /// is kept; before it, each request is screened on a decision naming it.
    reusing: AtomicBool,
    /// Locked only to read or write an entry, never across an await.
    held: Mutex<Held>,
}

/// Each pair's entry, with the order they run out in and the order they are due a refresh in.
#[derive(Default)]
struct Held {
    entries: HashMap<Pair, Entry>,
    /// Each entry by the last time it may be attached.
    expiring: BTreeSet<(Millis, Pair)>,
    /// Each entry waiting for a refresh, by when it is due one.
    scheduled: BTreeSet<(Millis, Pair)>,
}

/// One pair's decision.
struct Entry {
    screening: Screening,
    /// The last time it may be attached: its bound less the margin a request may wait for its stamp.
    usable_until: Millis,
    /// When the node received it.
    fetched_at: Millis,
    /// The last request from the pair that used it or fetched it.
    last_request: Millis,
    /// Its key in `scheduled`, while it waits for a refresh.
    due_at: Option<Millis>,
    /// Whether a refresh of it is in flight.
    refreshing: bool,
}

impl Decisions {
    /// No decisions yet, checked against `config`'s source and bound.
    pub(crate) fn new(config: &AccessSection) -> Self {
        Self::with_capacity(config, CAPACITY)
    }

    /// `new` keeping at most `capacity` pairs.
    fn with_capacity(config: &AccessSection, capacity: usize) -> Self {
        let max_age = Millis::try_from(config.max_age.as_millis()).expect("validate bounds max_age");
        let timeout = Millis::try_from(config.timeout.as_millis()).unwrap_or(Millis::MAX);
        let turn = (max_age / 20).clamp(50, 1_000);
        let lead = timeout.saturating_add(turn);
        let refresh_limit = (config.max_in_flight.get().saturating_sub(config.reserved_in_flight) / 2).max(1);
        let (reusing, held) = (AtomicBool::new(false), Mutex::default());
        Self { source: config.source, max_age, lead, turn, capacity, refresh_limit, reusing, held }
    }

    /// The pair's decision, if one may be attached at `now`, counting the request as the pair's latest.
    pub(crate) fn hit(&self, account: AccountId, peer: IpAddr, now: Millis) -> Option<Screening> {
        let mut held = self.lock();
        let pair = (account, peer);
        let entry = held.entries.get_mut(&pair).filter(|entry| now <= entry.usable_until)?;
        entry.last_request = entry.last_request.max(now);
        let screening = entry.screening.clone();
        // An entry left unscheduled while its pair was idle waits for a refresh again.
        let due = (entry.due_at.is_none() && !entry.refreshing).then(|| self.refresh_at(entry));
        if let Some(due) = due {
            held.schedule(pair, due);
        }
        debug!(%account, observed_at = screening.observed_at, "screening decision reused");
        Some(screening)
    }

    /// Keeps `screening`, received at `now` for a request from the pair or a refresh of it, if it
    /// verifies and may be attached, and is no older than the pair's decision. Returns whether it is
    /// kept.
    pub(crate) fn store(
        &self,
        account: AccountId,
        peer: IpAddr,
        screening: &Screening,
        domain: &Eip712Domain,
        now: Millis,
        request: bool,
    ) -> bool {
        let usable_until = screening.observed_at.saturating_add(self.max_age - self.margin());
        let verified = screening.source == self.source
            && screening.request_hash.is_none()
            && screening.account == Some(account)
            && screening.payer.is_none()
            && screening.observed_at <= now.saturating_add(MAX_EVIDENCE_LEAD_MS)
            && eip712::recover(&eip712::screening_hash(domain, screening), &screening.signature) == Some(self.source);
        if !verified || now > usable_until {
            debug!(%account, verified, "screening decision not kept");
            return false;
        }
        let pair = (account, peer);
        let mut held = self.lock();
        held.purge(now);
        let last_request = if request { now } else { Millis::MIN };
        let full = held.entries.len() >= self.capacity;
        let (last_request, refreshing) = match held.entries.get_mut(&pair) {
            Some(kept) if kept.screening.observed_at > screening.observed_at => {
                kept.last_request = kept.last_request.max(last_request);
                return false;
            }
            Some(kept) => (kept.last_request.max(last_request), kept.refreshing),
            None if full => {
                debug!(%account, capacity = self.capacity, "screening decisions full: decision not kept");
                return false;
            }
            None => (last_request, false),
        };
        held.remove(pair);
        held.expiring.insert((usable_until, pair));
        let screening = screening.clone();
        let entry = Entry { screening, usable_until, fetched_at: now, last_request, due_at: None, refreshing };
        let due = self.refresh_at(&entry);
        held.entries.insert(pair, entry);
        if !refreshing {
            held.schedule(pair, due);
        }
        true
    }

    /// Up to `limit` pairs that sent a request within the bound and whose decision is due a refresh at
    /// `now`, each marked as refreshing until `refreshed`. A pair found idle waits unscheduled until
    /// its next request, and its decision runs out.
    pub(crate) fn due(&self, now: Millis, limit: usize) -> Vec<Pair> {
        let mut held = self.lock();
        held.purge(now);
        let mut due = Vec::new();
        while due.len() < limit {
            let Some(&(at, pair)) = held.scheduled.first().filter(|(at, _)| *at <= now) else { break };
            held.scheduled.remove(&(at, pair));
            let entry = held.entries.get_mut(&pair).expect("a scheduled pair is kept");
            entry.due_at = None;
            if now.saturating_sub(entry.last_request) <= self.max_age {
                entry.refreshing = true;
                due.push(pair);
            }
        }
        due
    }

    /// Ends the pair's refresh at `now`: one that kept a newer decision waits for its next refresh, and
    /// one that did not tries again on the next turn, while its decision lasts.
    pub(crate) fn refreshed(&self, pair: Pair, now: Millis) {
        let mut held = self.lock();
        let Some(entry) = held.entries.get_mut(&pair) else { return };
        entry.refreshing = false;
        let due = self.refresh_at(entry).max(now.saturating_add(self.turn));
        if entry.due_at.is_none() {
            held.schedule(pair, due);
        }
    }

    /// Whether decisions are kept and reused: once the API has seen the log at `V85`, as it does from then
    /// on, since a log's version only rises.
    pub(crate) fn reusing(&self) -> bool {
        self.reusing.load(Ordering::Relaxed)
    }

    /// Keeps and reuses decisions from now on.
    pub(crate) fn reuse(&self) {
        self.reusing.store(true, Ordering::Relaxed);
    }

    /// How long the refresh loop waits between turns: a twentieth of the bound, between 50 ms and 1 s.
    pub(crate) fn tick(&self) -> Duration {
        Duration::from_millis(u64::try_from(self.turn).expect("a turn is positive"))
    }

    /// The time a request may wait for its stamp after its decision is attached: a tenth of the bound.
    fn margin(&self) -> Millis {
        self.max_age / 10
    }

    /// When `entry`'s decision is due a refresh: two of `lead` before it stops being attached, so a
    /// failed attempt has time for another, and no sooner than half its attachable life after it
    /// arrived, so a decision observed long before it arrived is not asked for again at every turn.
    /// Either way before it stops being attached.
    fn refresh_at(&self, entry: &Entry) -> Millis {
        let half_life = entry.usable_until.saturating_sub(entry.fetched_at) / 2;
        entry.usable_until.saturating_sub(2 * self.lead).max(entry.fetched_at.saturating_add(half_life))
    }

    /// The decisions, for one read or write.
    fn lock(&self) -> MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many pairs are kept.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Forgets every decision, as when each runs out, so a test's next request asks the source.
    #[cfg(test)]
    pub(crate) fn forget(&self) {
        *self.lock() = Held::default();
    }
}

impl Held {
    /// Forgets every entry that may no longer be attached at `now`.
    fn purge(&mut self, now: Millis) {
        while let Some(&(until, pair)) = self.expiring.first().filter(|(until, _)| *until < now) {
            self.expiring.remove(&(until, pair));
            if let Some(entry) = self.entries.remove(&pair)
                && let Some(due_at) = entry.due_at
            {
                self.scheduled.remove(&(due_at, pair));
            }
        }
    }

    /// Forgets the pair's entry, if any.
    fn remove(&mut self, pair: Pair) {
        if let Some(entry) = self.entries.remove(&pair) {
            self.expiring.remove(&(entry.usable_until, pair));
            if let Some(due_at) = entry.due_at {
                self.scheduled.remove(&(due_at, pair));
            }
        }
    }

    /// Schedules the pair's refresh at `at`.
    fn schedule(&mut self, pair: Pair, at: Millis) {
        if let Some(entry) = self.entries.get_mut(&pair) {
            entry.due_at = Some(at);
            self.scheduled.insert((at, pair));
        }
    }
}

#[cfg(test)]
mod tests;
