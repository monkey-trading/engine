//! A promoted operator's watch over its promotion (#595): until a final block records it, the
//! schedule's assignment for its turn and position, immutable once made, must name this key with the
//! same signer epoch, generation and commitments at every latest agreed block at or past the one the
//! start read it at. Once one lacks it, the promotion was reorged away or replaced: the node raises
//! Critical `promotion_reorged`, stops signing, marks the reorg in the promotion's record, and stops.
//! A latest block older than the start's, as a lagging provider majority serves, says nothing of it.

use std::path::Path;
use std::time::Duration;

use engine_core::types::AccountId;
use futures_util::future::BoxFuture;
use tracing::{debug, error, info};

use crate::liveness::Gate;
use crate::node::member::promoted::Record;
use crate::standby_key::chain::{AgreedBlock, Promotion, StandbyChain, StandbyDomain, promotion_of};

/// How long the watch waits between its reads.
const EVERY: Duration = Duration::from_secs(5);

/// What a promotion's watch reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// The latest agreed block, at or past the one the promotion was read at, lacks the promotion, or
    /// records other commitments.
    Missing,
    /// The latest agreed block records it, or is older than the one it was read at; no final block
    /// records it yet.
    Standing,
    /// A final block records it.
    Final,
}

/// What the watch compares the chain with: the promotion that made `key` the vault's signer in
/// `domain`, and the first seq sequenced under it.
pub(crate) struct Watched {
    pub domain: StandbyDomain,
    pub key: AccountId,
    pub promotion: Promotion,
    pub from_seq: u64,
}

/// Watches `chain` until a final block records the promotion (then never returns), or a latest block
/// lacks it: then it stops `gate`, marks the reorg in the record in `sealing`, and ends.
/// Error cases:
/// - a latest block lacks the promotion
pub(crate) async fn watch(
    chain: &impl StandbyChain,
    watched: &Watched,
    gate: &Gate,
    sealing: Option<&Path>,
) -> anyhow::Result<()> {
    let Watched { domain, key, promotion, from_seq } = watched;
    loop {
        tokio::time::sleep(EVERY).await;
        let read: BoxFuture<'_, anyhow::Result<Settled>> = Box::pin(settled(chain, domain, *key, promotion));
        match read.await {
            Ok(Settled::Final) => {
                info!(turn = promotion.turn, "a final block records the promotion");
                return std::future::pending().await;
            }
            Ok(Settled::Standing) => {}
            Ok(Settled::Missing) => {
                gate.stop();
                // Kept, so no start resumes it on a later block's record before a final one's.
                if let Some(dir) = sealing
                    && let Err(e) = Record::mark_reorged(dir)
                {
                    error!("recording the promotion's reorg in its record failed: {e:#}");
                }
                error!(
                    alert = "promotion_reorged",
                    turn = promotion.turn,
                    position = promotion.position,
                    "the latest block no longer records the promotion that made this key the vault's signer, with \
                     its commitments: signing stops; the history sequenced from seq {from_seq} waits for a final \
                     block to settle the promotion at the next start"
                );
                anyhow::bail!("the promotion of turn {} was reorged away or replaced", promotion.turn);
            }
            Err(e) => debug!("reading the promotion failed: {e:#}"),
        }
    }
}

/// What `chain` records of `promotion` of `key` in `domain` (see `Settled`).
/// Error cases:
/// - a read fails
pub(crate) async fn settled(
    chain: &impl StandbyChain,
    domain: &StandbyDomain,
    key: AccountId,
    promotion: &Promotion,
) -> anyhow::Result<Settled> {
    let recorded = |found: Option<Promotion>| found.is_some_and(|found| found.same(promotion));
    let latest = chain.latest().await?;
    if latest.number() < promotion.block {
        return Ok(Settled::Standing);
    }
    if !recorded(promotion_of(&latest, domain, key, promotion.turn, promotion.position).await?) {
        return Ok(Settled::Missing);
    }
    drop(latest);
    let finalized = chain.finalized().await?;
    Ok(match recorded(promotion_of(&finalized, domain, key, promotion.turn, promotion.position).await?) {
        true => Settled::Final,
        false => Settled::Standing,
    })
}
