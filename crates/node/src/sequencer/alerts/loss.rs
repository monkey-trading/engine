//! The alert for a house that has lost money to its clients over the trailing day, from `V59`.

use engine_core::Fixed;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::throttle::{REALERT_AFTER, Throttle};

/// The house loss trigger's level after the last command, the throttle on its alert, and the alerted
/// episode not yet resolved, with since when the level has been at zero. A new process holds an
/// episode open, since one may be from before a restart: a level above zero pages it again, which
/// the router takes as a repeat, and a level at zero resolves it.
pub(in crate::sequencer) struct LossAlert {
    level: usize,
    throttle: Throttle,
    alerted: bool,
    cleared_since: Option<Instant>,
}

impl Default for LossAlert {
    fn default() -> Self {
        Self { level: 0, throttle: Throttle::default(), alerted: true, cleared_since: None }
    }
}

impl LossAlert {
    /// Takes the loss trigger's `level` after a command: raises `house_loss_stress` when it rises
    /// above zero, with the loss `loss` reads, at most once per `REALERT_AFTER` while the loss keeps
    /// crossing its threshold, and resolves it once the level has stayed at zero for
    /// `REALERT_AFTER`, so a loss hovering at the threshold does not churn pages. Every priced market
    /// already pays the level; how fast the house loses is the operator's to look at.
    pub(in crate::sequencer) fn note(&mut self, level: usize, loss: impl FnOnce() -> Fixed) {
        let now = Instant::now();
        if level == 0 {
            if self.level > 0 || self.cleared_since.is_none() {
                self.cleared_since = Some(now);
                debug!("the house's loss over the trailing day is at or below its threshold");
            }
            self.level = 0;
            let cleared = self.cleared_since.is_some_and(|since| now.duration_since(since) >= REALERT_AFTER);
            if self.alerted && cleared {
                info!(
                    alert_resolved = "house_loss_stress",
                    "the house's loss over the trailing day has stayed at or below its threshold"
                );
                self.alerted = false;
            }
            return;
        }
        self.cleared_since = None;
        if self.level == 0
            && let Some(suppressed) = self.throttle.ready(now)
        {
            self.alerted = true;
            // The loss and level change while it lasts, so they go in the message.
            warn!(
                alert = "house_loss_stress",
                "the house lost {} to its clients over the trailing day, so every priced market rises to stress \
                 level {level} ({suppressed} more since the last alert)",
                loss()
            );
        }
        self.level = level;
    }
}
