//! On-chain log-head commitments (#541). The enclave signs its newest released `(seq, link)` —
//! the cumulative command-prefix digest after the log's last command — and a relayer posts it to
//! the LogHeads companion contract, chained to the last head a quorum read confirmed on chain. A
//! node restarting from a log the posted head does not hold refuses to start, naming the range, so
//! a host that truncates the log cannot resume the node unnoticed past the posting interval plus
//! inclusion. A detected rollback at runtime freezes sequencing. Head reads go through the custody
//! RPC quorum at a strict-majority latest block; the single posting endpoint is the operator's.

pub(crate) mod prefix;
mod read;
pub(crate) use read::PostedHeads;
mod rpc;
pub(crate) use rpc::StartupHeads;
#[cfg(test)]
pub(crate) use rpc::{posted, startup_heads};
pub(crate) mod setup;
mod submission;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloy::primitives::B256;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::log::CheckpointDomain;
use crate::sequencer::Request;
use crate::tee::EnclaveKey;

/// A rollback freeze latch shared between the log-head poster and the sequencer. The poster sets
/// it the moment it detects a rollback, before its `Freeze` request dequeues behind work already
/// queued, so nothing is stamped after the detection — the ordinary request queue cannot delay it.
#[derive(Clone, Default)]
pub(crate) struct FreezeLatch(Arc<AtomicBool>);

impl FreezeLatch {
    /// Latches sequencing frozen: everything queued behind the detection is refused.
    pub(crate) fn freeze(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether a rollback froze sequencing.
    pub(crate) fn frozen(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What the sequencer hands the on-chain head poster: the released-head watch and the rollback
/// freeze latch the poster shares with it, so the poster's detection refuses what is queued at
/// once, before its `Freeze` request travels the ordinary queue.
pub(crate) struct Publisher {
    pub tx: watch::Sender<Head>,
    pub freeze: FreezeLatch,
}

/// The newest released log head: the seq of the last durable entry and the cumulative
/// command-prefix digest after it — a running hash over every command's sealed bytes that
/// checkpoints do not reset, so a posted head commits to the whole history. The default is never
/// posted: seq 0 is the unattested empty log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Head {
    pub seq: u64,
    pub link: B256,
}

/// Chain access the poster reads and posts through.
pub(crate) trait HeadsChain: Send + Sync {
    /// The vault's current signer epoch and its newest posted head, read through the custody RPC
    /// quorum at one agreed block.
    /// Error cases:
    /// - the quorum is lost or a read fails
    async fn posted(&self) -> anyhow::Result<(u64, Option<Head>)>;

    /// Relays one signed head under the poster's dedicated relayer, naming the previously accepted
    /// head it extends (`None` for the first post of the signer epoch). A send whose receipt is
    /// lost is safe: the next read detects the post, and the sender dedups an in-flight send.
    /// `Waiting` — a pending send still inside its bounded wait — is not progress: it neither
    /// paces nor proves anything.
    /// Error cases:
    /// - nonce allocation or the submission fails
    async fn post(&self, epoch: u64, head: Head, parent: Option<Head>, signature: &str) -> anyhow::Result<PostOutcome>;
}

/// What a post attempt did, so the poster paces and alarms on accepted sends only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PostOutcome {
    /// The signed head was handed to the endpoint.
    Accepted,
    /// A send is pending at the outstanding nonce and the bounded wait has not elapsed: nothing
    /// was sent.
    Waiting,
}

/// Posts the newest released head at the configured interval and freezes sequencing on a rollback.
pub(crate) struct Poster<R> {
    pub chain: R,
    pub key: Arc<EnclaveKey>,
    pub domain: CheckpointDomain,
    pub interval: Duration,
    pub retry: Duration,
    /// Set the moment a rollback is detected, before the `Freeze` request dequeues.
    pub freeze: FreezeLatch,
    /// The startup-quorum-authenticated posted head and the epoch it was read under: the seed for
    /// the poster's per-epoch confirmed head — the parent the first post after a restart names.
    pub confirmed: Option<(u64, Head)>,
    /// The seq of the head startup checked the log against (0 with none posted), and its command's
    /// stamp, or, with none posted, the vault-finalized checkpoint's: the stall alarm's starting
    /// point, so a restart neither counts that head as an advance nor resets the time since it.
    pub anchored: Option<(u64, Option<engine_core::types::Millis>)>,
    /// Trusted time, which the stall alarm measures the time since the anchored command against;
    /// `None` measures this process's time only.
    pub trusted: Option<crate::trusted_time::TrustedClock>,
    /// Each command's cumulative prefix from the authenticated replay, the highest seqs kept:
    /// evidence that this log holds an on-chain head. `setup` seeds it from the replayed log; the
    /// poster records every released head into it.
    pub prefix_ring: crate::log::PrefixRing,
    /// Posting intervals without the on-chain head advancing, past which the stall alert fires:
    /// 3, or 2 under rotation, where a successor checks the tail's head against the posted one.
    pub stall_intervals: u32,
}

impl<R: HeadsChain> Poster<R> {
    /// Records each released head as it changes, and reads the chain on the retry timer; posts at
    /// most once per interval once the head moves. A rollback — an on-chain head past this node's released head, or one
    /// naming another prefix at a seq this node retains — logs Critical `log_rollback` naming it,
    /// freezes the sequencer, and returns, which stops the node.
    /// Error cases:
    /// - a rollback is detected (the alert names the range)
    pub(crate) async fn run(
        self,
        mut released: watch::Receiver<Head>,
        sequencer: mpsc::Sender<Request>,
    ) -> anyhow::Result<()> {
        let mut retry = interval(self.retry);
        retry.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Gates posts, not reads: a moved head posts once the interval since the last send has
        // passed. The gate advances on every send attempt, before the result: a lost response still paced the
        // relayer, and the read-back confirms inclusion on later ticks.
        let mut posted_at: Option<tokio::time::Instant> = None;
        // When this process first sent a post: the carried stall time counts from one posting
        // interval after it, the bound the post had to be included in.
        let mut first_post: Option<tokio::time::Instant> = None;
        // Anchoring progress: the last time a quorum read observed the on-chain head advance, and,
        // until one does, trusted time since startup's anchor was stamped — the checked head's,
        // which neither a restart nor a log cut back to it resets, or, with no head posted, the
        // newest finalized checkpoint's. That carried time counts once this process's
        // first post has had a posting interval to be included, so a restart after a long stop
        // does not page before its first post lands. Past `stall_intervals` intervals with a newer
        // released head, the stall alert fires once per episode. Acceptance proves no inclusion, so it
        // never moves this clock.
        let mut progressed_at = tokio::time::Instant::now();
        let (mut on_chain_seq, mut anchor_ts) = self.anchored.unwrap_or_default();
        let mut stalled = false;
        // The last quorum-confirmed head of the current epoch: what the next post extends. Seeded
        // from the startup-authenticated posted head; a read adopts a head only when its link is
        // the cumulative prefix this log holds at its seq. A same-key fork signs correctly too, so
        // the key proves nothing here: a mismatch freezes, and no evidence keeps the parent.
        let (mut confirmed, mut confirmed_epoch) =
            self.confirmed.map_or((None, 0), |(epoch, head)| (Some(head), epoch));
        // The retained ancestry, two bounded sets of this log's (seq, prefix) pairs. The anchors
        // are the heads a reorg or a late inclusion can put on chain as ours: the
        // startup-authenticated head and every head this process signs. The released ring is
        // seeded from the replay and extended with every released head, so it also covers a head
        // the previous process signed shortly before it stopped.
        let mut anchors = crate::log::PrefixRing::default();
        if let Some(startup) = confirmed {
            anchors.record(startup.seq, startup.link);
        }
        let mut released_ring = self.prefix_ring;
        // Whether the no-evidence warning for the currently observed head has been logged.
        let mut unfamiliar = false;
        // Transport failure episodes: each warns once and resolves on the next success, with the
        // changing seqs in the message so the episode key stays stable.
        let mut read_failed = false;
        let mut post_failed = false;
        loop {
            // A release only extends the evidence; reads run on the retry cadence, whatever the
            // release rate, so the poster never adds more than one quorum read per tick to the
            // custody providers' load.
            let read = tokio::select! {
                _ = retry.tick() => true,
                changed = released.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    false
                }
            };
            let head = *released.borrow();
            if head.seq == 0 {
                continue;
            }
            released_ring.record(head.seq, head.link);
            if !read {
                continue;
            }
            let (epoch, on_chain) = match self.chain.posted().await {
                Ok(posted) => posted,
                Err(cause) => {
                    warn!(alert = "log_head_read_failed", "reading the posted log head failed: {cause:#}");
                    read_failed = true;
                    continue;
                }
            };
            if std::mem::take(&mut read_failed) {
                info!(alert_resolved = "log_head_read_failed", "reading the posted log head recovered");
            }
            if epoch != confirmed_epoch {
                // A signer rotation starts a new chain: the epoch's first post names no parent.
                // The retained ancestry stays: a cumulative prefix commits to log history, not to
                // a signer epoch.
                confirmed_epoch = epoch;
                confirmed = None;
            }
            if let Some(posted) = on_chain {
                if posted.seq > on_chain_seq {
                    on_chain_seq = posted.seq;
                    (progressed_at, anchor_ts) = (tokio::time::Instant::now(), None);
                    if std::mem::take(&mut stalled) {
                        info!(alert_resolved = "log_head_stalled", "the on-chain log head advanced");
                    }
                }
                if posted.seq > head.seq {
                    error!(
                        alert = "log_rollback",
                        posted_seq = posted.seq,
                        posted_link = %posted.link,
                        our_seq = head.seq,
                        "the on-chain log head runs ahead of this node over seqs {}..={}: rollback or a \
                         second copy on this key; sequencing frozen",
                        head.seq + 1,
                        posted.seq
                    );
                    self.freeze.freeze();
                    let _ = sequencer.send(Request::Freeze).await;
                    bail!("on-chain log head at seq {} overtakes this log at seq {}", posted.seq, head.seq);
                }
                match anchors.prefix_at(posted.seq).or_else(|| released_ring.prefix_at(posted.seq)) {
                    Some(expected) if expected == posted.link => {
                        unfamiliar = false;
                        // The chain holds a head this log holds: the next post extends it,
                        // whether the read advanced past the confirmed head or a same-epoch reorg
                        // (reads are at `latest`) restored an ancestor.
                        confirmed = Some(posted);
                        if posted.seq == head.seq {
                            // Confirmed, not reposted: the interval paces only the next send.
                            continue;
                        }
                    }
                    Some(expected) => {
                        error!(
                            alert = "log_rollback",
                            seq = posted.seq,
                            posted_link = %posted.link,
                            our_link = %expected,
                            "the on-chain log head at seq {} names a cumulative prefix this log does \
                             not hold: a second copy on this key sequenced a fork; sequencing frozen",
                            posted.seq
                        );
                        self.freeze.freeze();
                        let _ = sequencer.send(Request::Freeze).await;
                        bail!("on-chain log head at seq {} names a prefix foreign to this log", posted.seq);
                    }
                    None => {
                        // No retained evidence at this seq: keep the current parent. The stale
                        // parent fails the contract's chaining rule and the stall alarm names
                        // the halt; a restart checks the posted head against the whole log.
                        if !unfamiliar {
                            unfamiliar = true;
                            warn!(seq = posted.seq, link = %posted.link, "the on-chain log head names a \
                                seq this node retains no prefix for: keeping the current parent until a \
                                restart checks it against the whole log");
                        }
                    }
                }
            } else if confirmed.is_some() {
                // The epoch reads empty: a reorg removed even the confirmed head. The next post
                // goes parentless under the first-post rule.
                confirmed = None;
            }
            let carried = first_post.is_some_and(|at| at.elapsed() >= self.interval);
            let since_anchor =
                anchor_ts.filter(|_| carried).zip(self.trusted.as_ref().and_then(|clock| clock.now_ms()));
            let since_anchor = since_anchor.map_or(0, |(at, now)| u64::try_from(now.saturating_sub(at)).unwrap_or(0));
            let unanchored = progressed_at.elapsed().max(Duration::from_millis(since_anchor));
            let stall_after = self.interval.saturating_mul(self.stall_intervals);
            if !stalled && head.seq > on_chain_seq && unanchored > stall_after {
                stalled = true;
                warn!(
                    alert = "log_head_stalled",
                    "the on-chain log head has not advanced past seq {on_chain_seq} for over {:?} while \
                     this node released seq {}: the pending send may be dropped and is reconciled on \
                     the next attempt",
                    stall_after,
                    head.seq
                );
            }
            // The contract holds no head at or past ours: post ours if the interval since the last
            // send allows. A failed send still paced the gate; the read-back confirms inclusion.
            if posted_at.is_some_and(|at| at.elapsed() < self.interval) {
                continue;
            }
            if confirmed == Some(head) {
                // A head cannot extend itself: the chain either holds it (handled above) or a
                // reorg replaced it with a head this node retains no evidence for — the stall,
                // not a self-parent post.
                continue;
            }
            // Signed, the head can land whatever the send's outcome — a lost response, a write
            // that outran its bound, a crash after the broadcast — so it is an anchor from here.
            anchors.record(head.seq, head.link);
            let signature = self.key.sign_log_head(
                self.domain.chain_id,
                &self.domain.vault.0,
                epoch,
                head.seq,
                &head.link.0,
                confirmed.map(|parent| (parent.seq, parent.link)),
            );
            posted_at = Some(tokio::time::Instant::now());
            first_post = first_post.or(posted_at);
            match self.chain.post(epoch, head, confirmed, &signature).await {
                // Acceptance proves no inclusion: only a quorum-observed advance is anchoring
                // progress, so the stall timer stands.
                Ok(PostOutcome::Accepted) => {
                    if std::mem::take(&mut post_failed) {
                        info!(alert_resolved = "log_head_post_failed", "posting the log head recovered");
                    }
                }
                Ok(PostOutcome::Waiting) => {}
                Err(cause) => {
                    post_failed = true;
                    warn!(alert = "log_head_post_failed", "posting the log head failed: {cause:#}")
                }
            }
        }
    }
}

/// Compares the posted head with the recovered log before anything is written. `link_at` returns
/// the link the replay captured at the posted seq from the bytes it authenticated, or `None`
/// where the seq sits at or under an attested snapshot base: the snapshot determines the content
/// there, and so the link, without the replay reaching it.
/// Error cases:
/// - the log ends before the posted head, the replay holds no command at the posted seq, or the
///   posted link differs at the posted seq
pub(crate) fn check_rollback(
    posted: Head,
    last_seq: u64,
    link_at: impl FnOnce(u64) -> anyhow::Result<Option<B256>>,
) -> anyhow::Result<()> {
    if posted.seq > last_seq {
        error!(
            alert = "log_rollback",
            posted_seq = posted.seq,
            log_seq = last_seq,
            "the on-chain log head names seqs {}..={} this log does not hold: refusing to start",
            last_seq + 1,
            posted.seq
        );
        bail!("log lacks the posted head's seqs {}..={}", last_seq + 1, posted.seq);
    }
    if let Some(link) = link_at(posted.seq).context("locating the posted head in the log")?
        && link != posted.link
    {
        error!(
            alert = "log_rollback",
            seq = posted.seq,
            posted_link = %posted.link,
            log_link = %link,
            "the on-chain log head's link differs at seq {}: refusing to start",
            posted.seq
        );
        bail!("the log's link at the posted head's seq {} differs", posted.seq);
    }
    Ok(())
}
