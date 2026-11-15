//! A member's copy whose tail the operator in effect's log refuses as a rollback, when a final block
//! shows no log will ever extend it: an orphaned promotion's tail (commands a promoted key sealed
//! under a promotion a reorg removed), or a superseded one (commands an ended signer epoch's key
//! sealed past the head that epoch posted last, or any it sealed when it posted none, which the
//! promotion that ended it fixed with the vault's checkpoint and the successor's log holds: a failed
//! operator's unconfirmed tail, restarted as a member). The member
//! sets the copy aside as evidence and rebuilds it from that operator's archive.

use std::ffi::OsString;
use std::path::Path;

use alloy::primitives::{Address, B256};
use anyhow::Context;
use engine_core::types::{AccountId, Millis};
use futures_util::future::BoxFuture;
use tracing::{error, warn};

use crate::custody::StandbyReader;
use crate::liveness::chain::{StoredHead, stored_head};
use crate::node::member::standby::applies;
use crate::node::member::{Member, PostedHead, Sampling};
use crate::sequencer::replay::LiveReplay;
use crate::standby_key::chain::{AgreedBlock, StandbyChain, recent_signers};

/// How far past the last stamp of a copy's tail a final block must be before the tail's key counts as
/// one no promotion named: block times may trail true time, and a promotion's block comes before its
/// member's first stamp.
const ORPHAN_MARGIN_S: u64 = 300;

/// What a final block shows of a copy's tail that the followed enclave's log refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tail {
    /// A key no recent signer epoch names sealed it (`orphaned`).
    Orphaned,
    /// The key of signer epoch `epoch`, which has ended, sealed it past `head_seq`, the head that
    /// epoch posted last (`superseded`).
    Superseded { epoch: u64, head_seq: u64 },
}

/// Whether a copy whose last command `sealer` sealed at `stamp_ms`, refused by the log of the followed
/// enclave `followed`, holds an orphaned promotion's tail: another key, which the vault at a final block
/// `ORPHAN_MARGIN_S` past the stamp, at `final_time_s`, names in none of its recent epochs (`named`).
fn orphaned(sealer: AccountId, followed: AccountId, stamp_ms: Millis, final_time_s: u64, named: &[AccountId]) -> bool {
    let settled =
        u64::try_from(stamp_ms / 1000).is_ok_and(|stamp| final_time_s >= stamp.saturating_add(ORPHAN_MARGIN_S));
    sealer != followed && settled && !named.contains(&sealer)
}

/// The signer epoch a copy's tail was sealed in, once it has ended: the newest of `epochs` (the vault's
/// recent signer epochs at a final block, oldest first) naming `sealer`, below the vault's `latest`.
fn ended_epoch(sealer: AccountId, latest: u64, epochs: &[(u64, AccountId)]) -> Option<u64> {
    let named = epochs.iter().rev().find(|(_, signer)| *signer == sealer);
    named.map(|(epoch, _)| *epoch).filter(|epoch| *epoch < latest)
}

/// Whether a copy ending at `last_seq`, holding the prefix `held` at the seq of `head`, the head an
/// ended epoch posted last, runs past that head on its history: `LogHeads` takes no head of an epoch
/// once it has ended, and a promotion fixes this one, so nothing past it binds a successor. An epoch
/// that posted no head bound nothing past the vault's checkpoint its promotion fixed, which the
/// successor's log holds: any tail it sealed is superseded.
fn superseded((head_seq, head_link): (u64, B256), held: Option<B256>, last_seq: u64) -> bool {
    head_seq == 0 || (last_seq > head_seq && held == Some(head_link))
}

/// What a final block shows of the vault's recent signers, and of the last head of the epoch a copy's
/// tail was sealed in, once that epoch has ended.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Final {
    /// The block's time, in seconds.
    time: u64,
    /// The keys the vault names in its recent signer epochs.
    named: Vec<AccountId>,
    /// The ended epoch, and the newest head `LogHeads` holds for it.
    ended: Option<(u64, StoredHead)>,
}

/// What the final block `block` shows for a tail `sealer` sealed, with the vault and `LogHeads` at
/// `contracts`.
/// Error cases:
/// - a read fails
async fn read_final(
    block: &impl AgreedBlock,
    (vault, log_heads): (Address, Address),
    sealer: AccountId,
) -> anyhow::Result<Final> {
    let (latest, epochs) = recent_signers(block, vault).await?;
    let ended = match ended_epoch(sealer, latest, &epochs) {
        Some(epoch) => Some((epoch, stored_head(block, log_heads, epoch).await?)),
        None => None,
    };
    Ok(Final { time: block.time(), named: epochs.into_iter().map(|(_, signer)| signer).collect(), ended })
}

/// What `seen` shows of the tail of `replica`, which the log of the followed enclave `followed`
/// refuses: `None` when neither rule sets it aside.
fn tail_of(seen: &Final, replica: &LiveReplay, followed: AccountId) -> Option<Tail> {
    let (sealer, last_seq) = (replica.tip_sealer()?, replica.last_seq()?);
    let stamp = replica.link_at(last_seq)?.ts;
    if sealer == followed {
        return None;
    }
    if orphaned(sealer, followed, stamp, seen.time, &seen.named) {
        return Some(Tail::Orphaned);
    }
    let (epoch, head) = seen.ended?;
    let held = replica.link_at(head.seq).map(|link| link.link);
    superseded((head.seq, head.link), held, last_seq).then_some(Tail::Superseded { epoch, head_seq: head.seq })
}

/// Moves the copy at `dir` aside to `<dir>.<suffix>` and syncs its parent, returning where it went.
/// Error cases:
/// - the rename or the sync fails
fn set_aside(dir: &Path, suffix: &str) -> std::io::Result<OsString> {
    let mut aside = dir.as_os_str().to_owned();
    aside.push(format!(".{suffix}"));
    std::fs::rename(dir, &aside)?;
    dir.parent().map_or(Ok(()), |parent| std::fs::File::open(parent)?.sync_all())?;
    Ok(aside)
}

impl<C: crate::checkpoints::CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Sets the copy aside when the operator in effect's log refuses it and a final block shows its tail
    /// is an orphaned promotion's or a superseded epoch's (see `Tail`): the member then rebuilds its copy
    /// from that operator's archive, and the set-aside copy keeps the tail as evidence. Raises Critical
    /// `promotion_reorged` for an orphaned tail and Warning `superseded_tail_set_aside` for a superseded
    /// one. Returns whether it did; a failed read leaves the copy, and the rollback page, as they were.
    pub(in crate::node::member) async fn set_aside_tail(
        &self,
        follower: &crate::standby::Follower,
        followed: AccountId,
    ) -> bool {
        let replica = follower.replica();
        let (Some(sealer), Some(seq)) = (replica.tip_sealer(), replica.last_seq()) else { return false };
        if sealer == followed || !applies(self.cfg) {
            return false;
        }
        let read = async {
            let log_heads = self.cfg.log_heads.as_ref().context("[log_heads]")?.contract;
            let chain = StandbyReader::connect_named(&self.cfg.custody, log_heads, "orphans")?;
            let vault = Address::from(self.cfg.custody.vault.parse::<AccountId>()?.0);
            let block = chain.finalized().await?;
            read_final(&block, (vault, log_heads), sealer).await
        };
        let read: BoxFuture<'_, anyhow::Result<Final>> = Box::pin(read);
        let tail = match read.await.map(|seen| tail_of(&seen, replica, followed)) {
            Ok(Some(tail)) => tail,
            Ok(None) => return false,
            Err(e) => {
                warn!("reading whether the copy's tail can be set aside failed: {e:#}");
                return false;
            }
        };
        let dir = &self.cfg.node.log_dir;
        let suffix = match tail {
            Tail::Orphaned => format!("orphaned-{seq}"),
            Tail::Superseded { .. } => format!("superseded-{seq}"),
        };
        let aside = match set_aside(dir, &suffix) {
            Ok(aside) => aside,
            Err(e) => {
                warn!(path = %dir.display(), "setting the copy aside failed: {e}");
                return false;
            }
        };
        let aside = Path::new(&aside).display();
        match tail {
            Tail::Orphaned => error!(
                alert = "promotion_reorged",
                key = %sealer,
                seq,
                "the copy's tail through seq {seq} was sealed by {sealer}, which a final block names in no signer \
                 epoch: a reorg removed the promotion that named it; the copy is set aside ({aside}) and rebuilt \
                 from the operator in effect's archive"
            ),
            Tail::Superseded { epoch, head_seq } => warn!(
                alert = "superseded_tail_set_aside",
                key = %sealer,
                epoch,
                seq,
                "the copy's commands {}..={seq} were sealed by {sealer} past the last head its signer epoch \
                 {epoch} posted, and a final block shows that epoch ended: the operator in effect's log does not \
                 hold them; the copy is set aside ({aside}) and rebuilt from that operator's archive",
                head_seq + 1
            ),
        }
        true
    }
}

#[cfg(test)]
mod tests;
