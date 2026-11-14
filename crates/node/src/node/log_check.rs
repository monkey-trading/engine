//! A node's part in the boundary's log check (`log_check`): the check its first start after a
//! handover records, the sequencer's setup, and the asking of a node that takes turns.

use std::sync::Arc;

use engine_core::eip712::B256;
use engine_core::types::AccountId;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::info;

use crate::config::NodeConfig;
use crate::log::CheckpointDomain;
use crate::log_check::ask::Asker;
use crate::log_check::{Due, VerifiedHead, own_candidates, own_check};
use crate::node::shutdown::TaskResult;
use crate::sequencer::{LogCheckSetup, ReplayedLog, Request};
use crate::tee::EnclaveKey;
use crate::tee::keys::Head;
use crate::tee::sealing::KeyStore;
use crate::trusted_time::TrustedClock;

/// Refuses the first start after a handover unless `log` ends at the predecessor's final checkpoint
/// (`keys::check_predecessor_head`), and seals, in the same write, the verified head after it: the
/// handed-over checkpoint, which `log`'s replay or replica verified, unless this release, or the
/// member its node runs for under the `operator` key, sequenced part of the range (`own_check`), as
/// the same member's upgrade did; then the head the keys carried stays.
/// Error cases:
/// - see `keys::check_predecessor_head`
pub(crate) fn after_handover(store: Option<&KeyStore>, operator: AccountId, log: &ReplayedLog) -> anyhow::Result<()> {
    let release = store.map(|store| B256::from(store.release().id()));
    let candidates = own_candidates(&log.replayed.engine, operator);
    let check = |previous: Option<VerifiedHead>, head: &Head| {
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
    let release = B256::from(store.release().id());
    (Some(LogCheckSetup { release, store: store.clone(), due }), Some(asking))
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
