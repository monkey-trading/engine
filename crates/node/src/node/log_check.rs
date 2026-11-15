//! A node's part in the boundary's log check (`log_check`): the check its first start after a
//! handover records, the sequencer's setup, and the asking of a node that takes turns.

use std::sync::Arc;

use anyhow::Context;
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::info;

use crate::config::NodeConfig;
use crate::log::CheckpointDomain;
use crate::log_check::ask::Asker;
use crate::log_check::{Due, VerifiedHead, applies, own_candidates, own_check};
use crate::node::shutdown::TaskResult;
use crate::sequencer::{LogCheckSetup, ReplayedLog, Request};
use crate::tee::EnclaveKey;
use crate::tee::keys::Head;
use crate::tee::sealing::KeyStore;
use crate::trusted_time::TrustedClock;

/// The checks a start records: after a handover, the handed-over checkpoint (`after_handover`); for a
/// start a promotion made, the vault's checkpoint at `promoted`, the seq the promotion fixed, which the
/// member's replica verified as it followed (`after_promotion`).
/// Error cases:
/// - see `after_handover` and `after_promotion`
pub(crate) fn after_start(
    store: Option<&KeyStore>,
    operator: AccountId,
    log: &ReplayedLog,
    promoted: Option<u64>,
) -> anyhow::Result<()> {
    after_handover(store, operator, log)?;
    match promoted {
        Some(checkpoint_seq) => after_promotion(store, operator, log, checkpoint_seq),
        None => Ok(()),
    }
}

/// Seals, for a start a promotion made, the verified head after the member's own verification of the
/// log through `checkpoint_seq`, the vault's checkpoint the promotion fixed, which its replica checked
/// entry by entry as it followed: counted, as a successor's at a handover, only while the gate applies
/// and neither this release nor the member its node runs for under the `operator` key sequenced part
/// of the range (`own_check`); otherwise the head the keys carry stays, and a following member's check
/// is asked for once this release has taken the turn. A log that no longer holds that checkpoint's
/// link or root, as a restart long past it, records nothing.
/// Error cases:
/// - the keys cannot be sealed again
pub(crate) fn after_promotion(
    store: Option<&KeyStore>,
    operator: AccountId,
    log: &ReplayedLog,
    checkpoint_seq: u64,
) -> anyhow::Result<()> {
    let replayed = &log.replayed;
    let Some(store) = store.filter(|_| applies(&replayed.engine)) else { return Ok(()) };
    let link = replayed.prefix_ring.prefix_at(checkpoint_seq);
    let (Some(link), Some(root)) = (link, replayed.checkpoint_root(checkpoint_seq)) else {
        return Ok(());
    };
    let head = VerifiedHead { seq: checkpoint_seq, link, root };
    let (release, candidates) = (B256::from(store.release().id()), own_candidates(&replayed.engine, operator));
    let previous = store.keys().log_check;
    let checked = own_check(previous, &replayed.takings, head, (release, &candidates));
    if checked == previous {
        return Ok(());
    }
    info!(seq = checkpoint_seq, "the promotion's check: this start verified the log through the vault's checkpoint");
    store.update(|keys| keys.log_check = checked).context("sealing the promotion's verified head")
}

/// Refuses the first start after a handover unless `log` ends at the predecessor's final checkpoint
/// (`keys::check_predecessor_head`), and seals, in the same write, the verified head after it: the
/// handed-over checkpoint, which `log`'s replay or replica verified, unless this release, or the
/// member its node runs for under the `operator` key, sequenced part of the range (`own_check`), as
/// the same member's upgrade did; then the head the keys carried stays. While the gate does not
/// apply the keys carry none, so a release built before the log check can still take them.
/// Error cases:
/// - see `keys::check_predecessor_head`
pub(crate) fn after_handover(store: Option<&KeyStore>, operator: AccountId, log: &ReplayedLog) -> anyhow::Result<()> {
    let release = store.map(|store| B256::from(store.release().id()));
    let candidates = own_candidates(&log.replayed.engine, operator);
    let gated = applies(&log.replayed.engine);
    let check = |previous: Option<VerifiedHead>, head: &Head| {
        if !gated {
            return None;
        }
        let (Some(release), Ok(root)) = (release, head.root.parse()) else { return previous };
        let handed = VerifiedHead { seq: head.seq, link: log.head_link(), root };
        let checked = own_check(previous, &log.replayed.takings, handed, (release, &candidates));
        if checked == Some(handed) {
            info!(
                seq = head.seq,
                "the handover's check: this start verified the log through the handed-over checkpoint"
            );
        }
        checked
    };
    crate::node::keys::check_predecessor_head(store, log.last_seq(), log.last_checkpoint(), check)
}

/// The sequencer's log check setup for a node whose sealed `store` takes turns, and the receiving end
/// of its checks due; neither without one.
pub(super) fn setup(store: Option<&Arc<KeyStore>>) -> (Option<LogCheckSetup>, Option<watch::Receiver<Option<Due>>>) {
    let Some(store) = store else { return (None, None) };
    let (due, asking) = watch::channel(None);
    (Some(LogCheckSetup { store: store.clone(), due }), Some(asking))
}

/// Spawns the asking for the checks `due` publishes, reading the vault `[checkpoints]` names, signing
/// each question with `key`, and verifying quotes against `store`'s platform at `trusted` time.
/// Error cases:
/// - the vault's client cannot be built, or the vault address does not parse
pub(super) fn spawn(
    cfg: &NodeConfig,
    tasks: &mut JoinSet<TaskResult>,
    (due, store): (Option<watch::Receiver<Option<Due>>>, Option<&Arc<KeyStore>>),
    key: &Arc<EnclaveKey>,
    sequencer: &mpsc::Sender<Request>,
    trusted: TrustedClock,
) -> anyhow::Result<()> {
    let (Some(due), Some(store), Some(vault)) = (due, store, crate::handover::vault_reader(cfg)?) else {
        return Ok(());
    };
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let platform = (domain, cfg.tee.provider, store.platform());
    let asker = Asker::new(vault, sequencer, due, platform, key.clone(), trusted);
    tasks.spawn(async move { ("log check", asker.run().await) });
    Ok(())
}
