//! A restart's recovered approvals not yet loaded. One whose latest authorization had expired by the log's
//! last command is dormant: no signer epoch and no restart signs it again unasked, so it is due only on its
//! account's request, and no later approval waits for it. The others, never signed or holding an
//! authorization still usable, are live: nothing after the oldest live one is signed until it is loaded. The
//! live ones load first, in FIFO order, then the dormant ones, so a restart's first new withdrawal waits
//! for the approvals of the last authorization lifetime, not for every approval the log holds.

use std::collections::VecDeque;

use crate::sequencer::Sequencer;
use crate::sequencer::checkpoints::BacklogEntry;

/// The recovered approvals not yet loaded, each kind in FIFO order.
#[derive(Default)]
pub(in crate::sequencer) struct Backlog {
    live: VecDeque<BacklogEntry>,
    dormant: VecDeque<BacklogEntry>,
}

impl FromIterator<BacklogEntry> for Backlog {
    /// A backlog of `entries`, all live until `set_aside` sorts them.
    fn from_iter<I: IntoIterator<Item = BacklogEntry>>(entries: I) -> Self {
        Self { live: entries.into_iter().collect(), dormant: VecDeque::new() }
    }
}

impl Backlog {
    /// Moves the live approvals whose latest authorization expired by `now`, in unix seconds, to the
    /// dormant ones, each kind keeping its order.
    fn set_aside(&mut self, now: u64) {
        let expired = |(_, _, latest): &BacklogEntry| {
            latest
                .as_ref()
                .and_then(|withdrawal| withdrawal.authorization.as_ref())
                .is_some_and(|auth| auth.expires_at <= now)
        };
        let (dormant, live) = std::mem::take(&mut self.live).into_iter().partition(expired);
        (self.live, self.dormant) = (live, dormant);
    }

    /// The next approval to load: the oldest live one, then the oldest dormant one.
    pub(in crate::sequencer) fn pop_front(&mut self) -> Option<BacklogEntry> {
        self.live.pop_front().or_else(|| self.dormant.pop_front())
    }

    /// The oldest live approval not yet loaded, which every later release waits for.
    pub(in crate::sequencer) fn oldest_live(&self) -> Option<&BacklogEntry> {
        self.live.front()
    }

    /// Whether every recovered approval is loaded.
    pub(in crate::sequencer) fn is_empty(&self) -> bool {
        self.live.is_empty() && self.dormant.is_empty()
    }

    /// How many recovered approvals wait to be loaded.
    #[cfg(test)]
    pub(in crate::sequencer) fn len(&self) -> usize {
        self.live.len() + self.dormant.len()
    }

    /// The approvals waiting to be loaded, the live ones first.
    pub(in crate::sequencer) fn iter(&self) -> impl Iterator<Item = &BacklogEntry> {
        self.live.iter().chain(&self.dormant)
    }
}

impl Sequencer {
    /// Sets the recovered backlog's dormant approvals behind its live ones, as of the engine's last
    /// command, then loads its first window (`load_backlog`).
    /// Error cases:
    /// - as `load_backlog`
    pub(in crate::sequencer) fn recover_backlog(&mut self) -> anyhow::Result<()> {
        if let Some(gate) = &mut self.gate {
            gate.backlog.set_aside(u64::try_from(self.engine.now() / 1_000).unwrap_or_default());
        }
        self.load_backlog()
    }
}
