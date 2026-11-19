//! A member's venue feeds for its own turn: opened once trusted time is within `WARM_LEAD` of the
//! turn's boundary, kept while it takes the turn, and handed to the node it starts, which sends their
//! quotes from its first command (`price_feeds::Warm`). They close when it follows on instead, so a
//! member holds venue connections only around its own turns, and when its start takes the whole log,
//! which opens the node's own as a node with no copy does.

use engine_core::types::Millis;
use tracing::{info, warn};

use crate::checkpoints::CheckpointChain;
use crate::node::member::turn::Opened;
use crate::node::member::{Member, PostedHead, Sampling, Turn};
use crate::node::price_feeds::{Warm, market_hours, warm_due};

impl<C: CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Opens the venue feeds while this member is `taking` its turn, or once trusted time is within
    /// `WARM_LEAD` of `own_next`, its own next turn's boundary, and closes them otherwise. They judge
    /// their markets' hours by `copy`'s, if it is open. A failed open is logged and tried at the next
    /// check; the node opens its own feeds at its start either way.
    pub(super) async fn warm_for(&mut self, taking: bool, own_next: Option<Millis>, copy: Option<&Opened>) {
        let due = match own_next {
            _ if taking => true,
            Some(boundary) => self.reached().await.is_ok_and(|reached| warm_due(boundary, reached)),
            None => false,
        };
        if !due {
            if self.warm.take().is_some() {
                info!("closed the venue feeds opened for this member's turn");
            }
            return;
        }
        let Some(delivered) = self.delivered.filter(|_| self.warm.is_none()) else { return };
        let hours = copy.map(|(follower, ..)| market_hours(follower.replica().engine())).unwrap_or_default();
        match Warm::open(&self.cfg.feeds, delivered, hours) {
            Ok(warm) => {
                match (&warm, taking) {
                    (Some(_), false) => {
                        info!(boundary = ?own_next, "opened the venue feeds ahead of this member's turn")
                    }
                    (Some(_), true) => info!("opened the venue feeds as this member takes its turn"),
                    (None, _) => {}
                }
                self.warm = warm;
            }
            Err(e) => warn!("opening the venue feeds ahead of this member's turn failed: {e:#}"),
        }
    }

    /// Hands the venue feeds opened for this turn to the start `turn` makes, or closes them.
    pub(super) fn hand_feeds(&mut self, turn: &mut anyhow::Result<Turn>) {
        match turn {
            Ok(Turn::Replica(start)) => start.feeds = self.warm.take(),
            Ok(Turn::Hold | Turn::Stop) | Err(_) => self.warm = None,
        }
    }
}
