//! Episodes: an alert pages when it starts and when it is resolved, not each time it repeats.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::time::Instant;

use crate::alerts::{AlertLine, Page, Severity, Status};

/// Most episodes tracked at once. Past it, the one quiet longest is forgotten: a repeat of it pages
/// again, and its resolve pages nothing.
pub(super) const MAX_EPISODES: usize = 1024;

struct Episode {
    severity: Severity,
    last_seen: Instant,
}

/// Episodes by key. An entry outlives its quiet period so that a resolve logged long after its
/// alert still reaches the alert's route; only a resolve ends it.
pub struct Episodes {
    quiet: Duration,
    tracked: BTreeMap<String, Episode>,
}

impl Episodes {
    #[cfg(test)]
    /// No episodes yet; an alert repeating within `quiet` of its last line does not page.
    pub fn new(quiet: Duration) -> Self {
        Self { quiet, tracked: BTreeMap::new() }
    }

    /// The episodes still open at `now` (key and severity) from before a restart, so their
    /// resolves still page.
    pub fn resumed(quiet: Duration, open: impl IntoIterator<Item = (String, Severity)>, now: Instant) -> Self {
        let tracked = open.into_iter().map(|(key, severity)| (key, Episode { severity, last_seen: now })).collect();
        Self { quiet, tracked }
    }

    /// The page `line` calls for at `now`, if it calls for one. An alert pages unless its key last
    /// fired within the quiet period. A resolve pages only when it ends a tracked episode, on that
    /// episode's severity; one with no open episode (a healthy tick re-asserting health) pages nothing.
    pub fn on_line(&mut self, line: AlertLine, now: Instant) -> Option<Page> {
        let key = line.key();
        let Some(severity) = line.severity else {
            let episode = self.tracked.remove(&key)?;
            return Some(Page::new(line, key, Status::Resolved, episode.severity));
        };
        if let Some(episode) = self.tracked.get_mut(&key) {
            let repeat = now.duration_since(episode.last_seen) < self.quiet;
            episode.last_seen = now;
            if repeat {
                return None;
            }
            episode.severity = severity;
            return Some(Page::new(line, key, Status::Firing, severity));
        }
        if self.tracked.len() >= MAX_EPISODES {
            let quietest = self.tracked.iter().min_by_key(|(_, episode)| episode.last_seen).map(|(k, _)| k.clone());
            if let Some(quietest) = quietest {
                self.tracked.remove(&quietest);
            }
        }
        self.tracked.insert(key.clone(), Episode { severity, last_seen: now });
        Some(Page::new(line, key, Status::Firing, severity))
    }

    /// Stops tracking `key`, whose page was not queued, so its next alert pages again.
    pub fn forget(&mut self, key: &str) {
        self.tracked.remove(key);
    }
}
