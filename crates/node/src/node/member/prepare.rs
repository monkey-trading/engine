//! A member's preparation before the registry turns rotation on (#595): its standby key, sealed,
//! answered for on the member link and vouched for, so the first members register before the switch.

use std::time::Duration;

use tracing::{info, warn};

use crate::node::member::standby::{Standby, applies};
use crate::node::member::{Member, PostedHead, Sampling};

/// How often a member preparing its standby key reads the registry's rotation switch again.
const PREPARE_EVERY: Duration = if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(30) };

impl<C: crate::checkpoints::CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Prepares this member's standby key while the registry's rotation switch is off, as the
    /// deployment does before it turns rotation on: opens or generates the key, answers on the member
    /// link, and writes its vouched registration once the operator in effect, or a member, vouches for
    /// it (`standby_unregistered` until the candidate sends it). Returns once the switch is on, with
    /// `chain_read` set at the first read of it.
    /// Error cases:
    /// - none: failed reads and vouches are retried
    pub(crate) async fn prepare_standby(&mut self, chain_read: &mut bool) {
        // Without the rule's sections there is no standby key to prepare: the rotation check that
        // follows decides at once.
        if !applies(self.cfg) {
            return;
        }
        let mut standby: Option<Standby> = None;
        loop {
            match self.vault.rotation_enabled().await {
                Ok(on) => {
                    *chain_read = true;
                    if on {
                        break;
                    }
                }
                Err(e) => warn!("reading the registry's rotation switch failed, trying again: {e:#}"),
            }
            {
                if standby.is_none() {
                    match Standby::open(self.cfg, &self.start, self.answering.clone(), self.metrics.clone()).await {
                        Ok((opened, _)) => standby = Some(opened),
                        Err(e) => warn!("opening the member's standby keys failed, trying again: {e:#}"),
                    }
                }
                if let Some(open) = standby.as_mut() {
                    info!("rotation is off: preparing this member's standby key");
                    let operator = crate::standby::resolve(self.cfg, &self.vault).await;
                    let operator = operator.inspect_err(|e| warn!("no operator in effect to vouch: {e:#}")).ok();
                    if let Ok(reached) = self.reached().await {
                        if let Some(sample) = self.sample {
                            open.sampled(sample);
                        }
                        let quoting = crate::handover::evidence::Quoting::from(&self.cfg.tee);
                        open.register(&quoting, self.attestor.as_ref(), reached, operator.as_ref()).await;
                    }
                }
            }
            tokio::time::sleep(PREPARE_EVERY).await;
        }
        if let Some(standby) = standby {
            // The member link frees the handover endpoint; following opens the standby side again.
            standby.stop().await;
        }
    }
}
