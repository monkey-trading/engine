//! A promoted member's start (#595). Once the schedule's promotion makes its standby key the vault's
//! signer, in a block the custody quorum agrees on, the member starts sequencing in process from its
//! copy, only if the copy holds both prefixes the promotion fixed: the outgoing epoch's latest head,
//! matched by link, and the vault's latest accepted checkpoint, matched by root. Lacking either, it
//! fetches the gap from the member whose enclave confirmed the head, then from every other member of
//! the turn's set, each entry checked against the copy's seal chain; if it still lacks one, it raises
//! Critical `promotion_start_refused` naming the range and keeps following, so the next promotion
//! passes it. It records the promotion, reseals the standby key as the turn's keys under the
//! promotion's generation before its first command, and starts.

use alloy::primitives::Address;
use anyhow::Context;
use engine_core::types::AccountId;
use serde::Serialize;
use tracing::{error, info, warn};

use crate::checkpoints::CheckpointChain;
use crate::liveness::link::{Asking, Reply, Request, Tcp, describe};
use crate::log::Offset;
use crate::node::member::standby::Standby;
use crate::node::member::turn::Opened;
use crate::node::member::{Member, PostedHead, Replica, Sampling};
use crate::node::ready::{Moved, Ready, Start};
use crate::sequencer::replay::LiveReplay;
use crate::standby::Follower;
use crate::standby_key::chain::Promotion;

/// The record a promoted member keeps of its promotion, beside its keys.
pub(crate) const RECORD: &str = "promotion.json";
/// How many rounds of `Entries` answers a gap fetch takes at most.
const FETCH_ROUNDS: usize = 64;

/// The promotion as the record carries it: what it fixed, and the turn assignment and signer epoch it
/// made, which a later start compares with what the chain finalised.
#[derive(Serialize)]
struct Record {
    candidate: AccountId,
    turn: u64,
    position: u16,
    signer_epoch: u64,
    generation: u64,
    head_seq: u64,
    head_link: String,
    checkpoint_seq: u64,
    checkpoint_root: String,
    /// The first seq this member sequences under the promotion.
    from_seq: u64,
}

impl<C: CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Starts this member as the operator `promotion` made it, from `opened`, its copy and replica.
    /// Error cases:
    /// - the copy lacks a prefix the promotion fixed once every member was asked for the gap (Critical
    ///   `promotion_start_refused`), or the reseal, the record or the start's reads fail
    pub(super) async fn start_promoted(
        &mut self,
        standby: Standby,
        promotion: Promotion,
        opened: Opened,
    ) -> anyhow::Result<Start> {
        let (mut follower, stats, history, marks) = opened;
        let before = follower.replica().last_seq().unwrap_or(0);
        if missing(follower.replica(), &promotion).is_some() {
            self.fetch_gap(&standby, &promotion, &mut follower).await;
        }
        if let Some(missing) = missing(follower.replica(), &promotion) {
            error!(
                alert = "promotion_start_refused",
                turn = promotion.turn,
                position = promotion.position,
                "the promoted member cannot start: its copy lacks {missing}, which no member could give it; the next \
                 promotion passes it"
            );
            standby.stop().await;
            anyhow::bail!("the copy lacks {missing}");
        }
        let tip = follower.replica().last_seq().unwrap_or(0);
        let candidate = standby.candidate();
        let dir = self.cfg.tee.sealing.as_ref().context("a member seals its keys")?.dir.clone();
        let record = Record {
            candidate,
            turn: promotion.turn,
            position: promotion.position,
            signer_epoch: promotion.signer_epoch,
            generation: promotion.generation,
            head_seq: promotion.head_seq,
            head_link: promotion.head_link.to_string(),
            checkpoint_seq: promotion.checkpoint_seq,
            checkpoint_root: promotion.checkpoint_root.to_string(),
            from_seq: tip + 1,
        };
        let bytes = serde_json::to_vec_pretty(&record).context("encoding the promotion record")?;
        crate::tee::sealing::write_replacing(&dir.join(RECORD), &bytes)?;
        let keys = standby.into_keys().await?;
        let signer = keys.address();
        // Resealed before the first command, so a restart opens the key as the turn's keys only.
        let store = keys.reseal(promotion.generation)?;
        let read = Ready::read_promoted(
            self.cfg,
            &self.start,
            &self.metrics,
            signer,
            candidate,
            follower.replica(),
            promotion,
            self.time.sample(),
        );
        let ready = read.await?;
        info!(turn = promotion.turn, position = promotion.position, %signer, after = tip,
            "promoted: starting from the copy under the standby key");
        let moved = Moved { after: before, entries: tip.saturating_sub(before), bytes: 0 };
        Ok(Start { replica: Replica { follower, stats, history, marks }, store, ready, moved })
    }

    /// Fetches what the copy lacks of `promotion`'s prefixes from the members of its turn's set: the
    /// member whose enclave confirmed the outgoing epoch's head first, as the operator's stream named it,
    /// then every other, round after round while one gives entries.
    async fn fetch_gap(&self, standby: &Standby, promotion: &Promotion, follower: &mut Follower) {
        let mut members = match standby.turn_members().await {
            Ok(members) => members,
            Err(e) => return warn!("reading the turn's members for the gap failed: {e:#}"),
        };
        let confirmer = standby
            .copy()
            .signed()
            .filter(|signed| signed.head.head.seq == promotion.head_seq)
            .map(|signed| signed.head.confirmer);
        members.retain(|member| member.candidate != standby.candidate() && !member.endpoint.is_empty());
        members.sort_by_key(|member| Some(Address::from(member.candidate.0)) != confirmer);
        for _ in 0..FETCH_ROUNDS {
            if missing(follower.replica(), promotion).is_none() {
                return;
            }
            let Some(tip) = follower.replica().last_seq() else { return };
            let mut progressed = false;
            for member in &members {
                let request = Request::Entries { after: Offset::start_of(tip) };
                let entries = match Tcp.ask(&member.endpoint, &request).await {
                    Ok(Reply::Entries { entries }) if !entries.is_empty() => entries,
                    Ok(other) => {
                        info!(member = %member.candidate, "the member has no entries past seq {tip}: {}", describe(&other));
                        continue;
                    }
                    Err(e) => {
                        warn!(member = %member.candidate, "asking a member for the gap failed: {e:#}");
                        continue;
                    }
                };
                let mut lines = Vec::new();
                for (_, entry) in &entries {
                    match serde_json::to_vec(entry) {
                        Ok(line) => {
                            lines.extend(line);
                            lines.push(b'\n');
                        }
                        Err(e) => return warn!("encoding a fetched entry failed: {e:#}"),
                    }
                }
                match follower.take_tail(tip, &lines) {
                    Ok(taken) => {
                        info!(member = %member.candidate, taken, after = tip, "took the gap from a member");
                        progressed = taken > 0;
                        break;
                    }
                    Err(e) => warn!(member = %member.candidate, "a member's entries do not extend the copy: {e:#}"),
                }
            }
            if !progressed {
                return;
            }
        }
    }
}

/// What the replica lacks of `promotion`'s prefixes, if anything: the outgoing epoch's latest head,
/// matched by link, and the vault's latest accepted checkpoint, matched by root.
pub(crate) fn missing(replica: &LiveReplay, promotion: &Promotion) -> Option<String> {
    let last = replica.last_seq().unwrap_or(0);
    if promotion.head_seq > 0 {
        match replica.link_at(promotion.head_seq) {
            Some(link) if link.link == promotion.head_link => {}
            Some(link) => {
                return Some(format!(
                    "the head at seq {} (it holds link {} there, not {})",
                    promotion.head_seq, link.link, promotion.head_link
                ));
            }
            None if last < promotion.head_seq => {
                return Some(format!("seqs {}..={} up to the outgoing epoch's head", last + 1, promotion.head_seq));
            }
            None => return Some(format!("the link at the outgoing epoch's head, seq {}", promotion.head_seq)),
        }
    }
    if promotion.checkpoint_seq > 0 {
        let root = format!("{:#x}", promotion.checkpoint_root);
        match replica.checkpoint_at(promotion.checkpoint_seq) {
            Some(checkpoint) if checkpoint.root.eq_ignore_ascii_case(&root) => {}
            Some(checkpoint) => {
                return Some(format!(
                    "the checkpoint at seq {} (it holds root {}, not {root})",
                    promotion.checkpoint_seq, checkpoint.root
                ));
            }
            None if last < promotion.checkpoint_seq => {
                return Some(format!(
                    "seqs {}..={} up to the vault's latest checkpoint",
                    last + 1,
                    promotion.checkpoint_seq
                ));
            }
            None => return Some(format!("the vault's latest checkpoint at seq {}", promotion.checkpoint_seq)),
        }
    }
    None
}

#[cfg(test)]
mod tests;
