//! The member's standby side beside its following: the record of an earlier promotion first, then
//! the standby keys, the member link, the liveness caller and the standby certificate, a promoted
//! start, and the key's registration.

use tracing::{info, warn};

use crate::handover::evidence::Quoting;
use crate::node::member::promoted::Resumed;
use crate::node::member::standby::{Standby, applies};
use crate::node::member::turn::{Opened, Stopping};
use crate::node::member::{Member, PostedHead, Sampling};
use crate::node::ready::Start;
use crate::standby::Followed;

impl<C: crate::checkpoints::CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Keeps the member's standby side beside its following: opens it once the rule applies, links the
    /// follower's copy to it, starts the member as the operator once a promotion makes its standby key
    /// the vault's signer, and otherwise registers the key while the schedule does not list it as
    /// promotable, asking `operator` first.
    /// Before the standby side opens, an earlier promotion's record decides first (`resume_promoted`):
    /// the member resumes as the operator, waits, or follows.
    /// Error cases:
    /// - the promoted start fails (see `start_promoted`), which consumes the standby side, or the copy
    ///   does not open for it
    pub(in crate::node::member) async fn keep_standby(
        &mut self,
        standby: &mut Option<Standby>,
        follower: &mut Option<Opened>,
        operator: Option<&Followed>,
    ) -> anyhow::Result<Kept> {
        if !applies(self.cfg) {
            return Ok(Kept::Follow);
        }
        let mut pending = None;
        if standby.is_none() {
            match self.resume_promoted().await {
                Ok(Resumed::Start(start)) => return Ok(Kept::Start(start)),
                Ok(Resumed::Wait) => return Ok(Kept::Wait),
                Ok(Resumed::None) => {}
                Err(e) if e.downcast_ref::<Stopping>().is_some() => return Err(e),
                Err(e) => {
                    // A record that stands unread must not let the member take a turn past it.
                    warn!("checking the promotion record failed, trying again: {e:#}");
                    return Ok(Kept::Wait);
                }
            }
            match Standby::open(self.cfg, &self.start, self.answering.clone(), self.metrics.clone()).await {
                Ok((opened, promotion)) => (*standby, pending) = (Some(opened), promotion),
                Err(e) => {
                    warn!("opening the member's standby keys failed, trying again: {e:#}");
                    return Ok(Kept::Follow);
                }
            }
        }
        let Some(open) = standby.as_mut() else { return Ok(Kept::Follow) };
        if let Err(e) = open.supervise(self.cfg).await {
            warn!("restarting the member link and the liveness caller failed, trying again: {e:#}");
        }
        if let Some((replica, ..)) = follower.as_mut()
            && !replica.linked()
        {
            replica.link_with(open.copy());
        }
        let promotion = match pending {
            Some(promotion) => Some(promotion),
            None => open.promotion().await.unwrap_or_else(|e| {
                warn!("reading whether a promotion named this member failed: {e:#}");
                None
            }),
        };
        if let Some(promotion) = promotion {
            info!(
                turn = promotion.turn,
                position = promotion.position,
                "a promotion made this member's standby key the vault's signer"
            );
            // With no copy open, one the host left empty bootstraps from the followed operator's
            // archive, as the follow loop's own first open does.
            let opened = match follower.take() {
                Some(opened) => opened,
                None => self.open(operator).await?,
            };
            let standby = standby.take().expect("open above");
            let start = self.start_promoted(standby, promotion, opened, operator).await?;
            return Ok(Kept::Start(Box::new(start)));
        }
        if let Ok(reached) = self.reached().await {
            if let Some(sample) = self.sample {
                open.sampled(sample);
            }
            open.register(&Quoting::from(&self.cfg.tee), self.attestor.as_ref(), reached, operator).await;
        }
        Ok(Kept::Follow)
    }
}

/// What the member does after its standby side's turn.
pub(in crate::node::member) enum Kept {
    /// Follow the operator.
    Follow,
    /// Start as the operator a promotion made it.
    Start(Box<Start>),
    /// Neither: an earlier promotion is unsettled.
    Wait,
}
