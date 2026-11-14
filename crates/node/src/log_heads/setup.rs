//! Startup wiring: read the posted head before the log replays, so the rollback check binds to the
//! link the replay captures from the bytes it authenticates, never a second pass over the host's
//! files; then check it before anything is written and hand the poster its receiver.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use engine_core::types::AccountId;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use super::{Head, Poster, StartupHeads, rpc::HeadsRpc};
use crate::config::NodeConfig;
use crate::custody::heads_reader;
use crate::log::CheckpointDomain;
use crate::sequencer::Request;
use crate::tee::EnclaveKey;

/// Posting intervals without an advance before `log_head_stalled`: three, or two with rotation on,
/// whose handover checks the tail's head against the latest posted one.
pub(super) fn stall_intervals(cfg: &NodeConfig) -> u32 {
    if cfg.operator.rotation_enabled { 2 } else { 3 }
}

/// The poster and both ends of the watch carrying released heads to it. The watch starts at the
/// recovered log's head, so the poster posts even before the first new batch flushes.
pub(crate) struct Setup {
    pub poster: Poster<HeadsRpc>,
    pub tx: watch::Sender<Head>,
    pub rx: watch::Receiver<Head>,
    /// The rollback freeze latch the poster shares with the sequencer.
    pub freeze: super::FreezeLatch,
}

impl Setup {
    /// The sequencer's publish end: the released-head watch and the shared freeze latch.
    pub(crate) fn publisher(&self) -> super::Publisher {
        super::Publisher { tx: self.tx.clone(), freeze: self.freeze.clone() }
    }

    /// Runs the poster as a supervised task until a rollback freezes it, its stall alarm reading
    /// `trusted` time.
    pub(crate) async fn spawn(
        mut self,
        sequencer: mpsc::Sender<Request>,
        trusted: crate::trusted_time::TrustedClock,
    ) -> (&'static str, anyhow::Result<()>) {
        self.poster.trusted = Some(trusted);
        ("log_heads", self.poster.run(self.rx, sequencer).await)
    }
}

/// The poster's clients, the rollback freeze latch, and the posted head the replay must capture
/// the link at.
pub(crate) struct Pending {
    poster: Poster<HeadsRpc>,
    posted: Option<Head>,
    freeze: super::FreezeLatch,
}

impl Pending {
    /// The newest head this key's epochs hold, which the log must contain, with the poster's
    /// confirmed parent seeded from the current epoch's head only: the first post after a restart
    /// names it, so a tail already past it does not fail the contract's chaining rule, and an
    /// earlier epoch's head is no parent under its first-post rule.
    fn from_heads(mut poster: Poster<HeadsRpc>, heads: super::rpc::StartupHeads, freeze: super::FreezeLatch) -> Self {
        poster.confirmed = heads.current.map(|head| (heads.epoch, head));
        if let Some((epoch, head)) = heads.newest {
            info!(epoch, seq = head.seq, "startup checks the log against the head signer epoch {epoch} posted");
        }
        Self { poster, posted: heads.newest.map(|(_, head)| head), freeze }
    }

    /// The posted head the replay must bind to, if one is posted.
    pub(crate) fn posted(&self) -> Option<Head> {
        self.posted
    }

    /// What `read_posted` returns when startup reads `heads`, over a chain access in `dir` that
    /// touches no network.
    #[cfg(test)]
    pub(crate) fn for_test_heads(heads: super::rpc::StartupHeads, dir: &std::path::Path) -> Self {
        let Self { poster, freeze, .. } = Self::for_test(None, dir);
        Self::from_heads(poster, heads, freeze)
    }

    /// What `read_posted` returns when the contract holds `posted` for the current epoch, over a
    /// chain access in `dir` that touches no network.
    #[cfg(test)]
    pub(crate) fn for_test(posted: Option<Head>, dir: &std::path::Path) -> Self {
        let freeze = super::FreezeLatch::default();
        let domain = CheckpointDomain { chain_id: 31_337, vault: engine_core::types::AccountId([7; 20]) };
        let poster = Poster {
            chain: super::tests::heads_rpc("http://127.0.0.1:9", dir),
            key: Arc::new(EnclaveKey::test_key()),
            domain,
            interval: Duration::from_secs(3600),
            retry: Duration::from_secs(1),
            freeze: freeze.clone(),
            confirmed: None,
            anchored: None,
            trusted: None,
            prefix_ring: Default::default(),
            stall_intervals: 3,
        };
        Self { poster, posted, freeze }
    }
}

/// Builds clients without network access and reads the posted head through the custody RPC
/// quorum, before the log replays: the rollback check binds to the replay's capture at the
/// posted seq, so the seq must be known when recovery reads the log.
/// Error cases:
/// - `[log_heads]` is set without `[checkpoints]` or EVM custody (config validation rules out),
///   a client or the nonce store cannot be built, or the read budget elapses with no quorum read
pub(crate) async fn read_posted(cfg: &NodeConfig, key: &Arc<EnclaveKey>) -> anyhow::Result<Option<Pending>> {
    let Some(heads) = read_heads(cfg, AccountId(key.address())).await? else { return Ok(None) };
    pending(cfg, key, heads).map(Some)
}

/// The heads startup checks `signer`'s log against, read through the custody RPC quorum, with
/// `[log_heads]`: a member reads them before it connects for its turn's tail, so its start reads
/// nothing after the confirmation.
/// Error cases:
/// - as `read_posted`
pub(crate) async fn read_heads(cfg: &NodeConfig, signer: AccountId) -> anyhow::Result<Option<StartupHeads>> {
    let Some(chain) = heads_chain(cfg)? else { return Ok(None) };
    let checkpoints = cfg.checkpoints.as_ref().context("[log_heads] requires [checkpoints]")?;
    let (retry, budget) = (
        checkpoints.retry_interval,
        cfg.custody.evm.as_ref().map_or(Duration::from_secs(30), |evm| evm.finality_stalled_after),
    );
    let signer = alloy::primitives::Address::from(signer.0);
    read_posted_with_retry(&chain, signer, retry, budget).await.map(Some)
}

/// The poster for `key` over `heads`, read already: it builds clients and reads nothing.
/// Error cases:
/// - `[log_heads]` is missing, or a client or the nonce store cannot be built
pub(crate) fn pending(cfg: &NodeConfig, key: &Arc<EnclaveKey>, heads: StartupHeads) -> anyhow::Result<Pending> {
    let chain = heads_chain(cfg)?.context("the posted heads were read without [log_heads]")?;
    let log_heads = cfg.log_heads.as_ref().context("[log_heads] is set")?;
    let checkpoints = cfg.checkpoints.as_ref().context("[log_heads] requires [checkpoints]")?;
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let freeze = super::FreezeLatch::default();
    let poster = Poster {
        chain,
        key: key.clone(),
        domain,
        interval: log_heads.interval,
        retry: checkpoints.retry_interval,
        freeze: freeze.clone(),
        confirmed: None,
        anchored: None,
        trusted: None,
        prefix_ring: Default::default(),
        stall_intervals: stall_intervals(cfg),
    };
    Ok(Pending::from_heads(poster, heads, freeze))
}

/// The log heads' chain access `[log_heads]` configures, built without network access.
/// Error cases:
/// - as `read_posted`
fn heads_chain(cfg: &NodeConfig) -> anyhow::Result<Option<HeadsRpc>> {
    let Some(log_heads) = &cfg.log_heads else { return Ok(None) };
    let checkpoints = cfg.checkpoints.as_ref().context("[log_heads] requires [checkpoints]")?;
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let quorum = Arc::new(heads_reader(&cfg.custody)?);
    HeadsRpc::new(checkpoints, log_heads, domain, &cfg.node.log_dir, quorum).map(Some)
}

/// Reads, through the custody RPC quorum, the current signer epoch, its posted head, and the newest
/// head this key's epochs hold (`rpc::startup_heads`), retrying transport failures up to the custody
/// finality budget: a node that cannot bound its log's rollback window must not open the log for
/// writing.
/// Error cases:
/// - the budget elapses with no quorum read
async fn read_posted_with_retry(
    chain: &HeadsRpc,
    signer: alloy::primitives::Address,
    retry: Duration,
    budget: Duration,
) -> anyhow::Result<StartupHeads> {
    tokio::time::timeout(budget, async {
        let mut failed = false;
        loop {
            match chain.startup_heads(signer).await {
                Ok(posted) => {
                    if std::mem::take(&mut failed) {
                        info!(alert_resolved = "log_head_read_failed", "reading the posted log head recovered");
                    }
                    return Ok(posted);
                }
                Err(cause) => {
                    warn!(alert = "log_head_read_failed", "reading the posted log head failed: {cause:#}");
                    failed = true;
                    tokio::time::sleep(retry).await;
                }
            }
        }
    })
    .await
    .context("awaiting the posted log head exceeded the custody finality budget")?
}

/// The startup gate and poster setup in one call: checks the posted head against the link the
/// replay captured — accepting it seq-only at or under an attested snapshot base, which determines
/// the content there — and builds the poster with the watch at the recovered head and its stall
/// alarm started from the checked head's stamp, or the vault-finalized checkpoint's with no head
/// posted. A detected rollback logs Critical `log_rollback` naming the range.
/// Error cases:
/// - the rollback check fails (its Critical names the range)
pub(crate) fn startup(pending: Option<Pending>, log: &crate::sequencer::ReplayedLog) -> anyhow::Result<Option<Setup>> {
    let Some(pending) = pending else { return Ok(None) };
    let Pending { mut poster, posted, freeze } = pending;
    if let Some(posted) = posted {
        let last_seq = log.last_seq().unwrap_or(0);
        let base = log.snapshot_seq;
        // `check_rollback` logs the Critical naming the range; the refusal line adds no second alert.
        super::check_rollback(posted, last_seq, |seq| {
            if base.is_some_and(|base| seq <= base) {
                // The suffix replay never reaches a seq at or under the snapshot base; the
                // candidate verified the posted cumulative prefix against the on-disk prefix under
                // the trusted authority before accepting the snapshot. The seq check binds here.
                return Ok(None);
            }
            log.replayed
                .link_at
                .map(Some)
                .with_context(|| format!("the replayed log holds no command at the posted head's seq {seq}"))
        })
        .inspect_err(|e| error!("refusing to serve: {e:#}"))?;
    }
    // The stall alarm starts from the checked head's stamp, or, with no head of this key posted,
    // the vault-finalized checkpoint's, not from this process: the checked head is no advance, and
    // trusted time since that stamp counts. A newer checkpoint does not move a head's anchor.
    let stamp = posted.map_or(log.replayed.anchor_ts, |_| log.replayed.ts_at);
    poster.anchored = Some((posted.map_or(0, |head| head.seq), stamp));
    // The evidence that this log holds an on-chain head: each command's cumulative prefix as the
    // replay authenticated it, seeded before the poster's first read.
    poster.prefix_ring = log.prefix_ring();
    // The watch starts at the recovered head: for a snapshot with an empty suffix that is the
    // certified cumulative prefix at the base (the value `prefix::verify` binds the on-disk log
    // to, and what a sync-only release republishes) — never the checkpoint link, which restarts
    // only the internal seal chain.
    let head = Head { seq: log.last_seq().unwrap_or(0), link: log.head_link() };
    let (tx, rx) = watch::channel(head);
    Ok(Some(Setup { poster, tx, rx, freeze }))
}
