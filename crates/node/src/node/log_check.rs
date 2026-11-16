//! A node's part in the boundary's log check (`log_check`): the check its first start after a
//! handover records, the sequencer's setup, and the asking of a node that takes turns.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Context;
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::info;

use crate::checkpoints::CheckpointChain;
use crate::config::NodeConfig;
use crate::log::CheckpointDomain;
use crate::log_check::ask::Asker;
use crate::log_check::{Due, Holder, Reassigned, VerifiedHead, applies, own_candidates, own_check_read, seal_verified};
use crate::node::shutdown::TaskResult;
use crate::sequencer::{LogCheckSetup, ReplayedLog, Request};
use crate::tee::EnclaveKey;
use crate::tee::keys::Head;
use crate::tee::sealing::KeyStore;
use crate::trusted_time::TrustedClock;

/// How many turns before a start's own the schedule's reassignments are read: an earlier reassignment is
/// final and mirrored, so the log's records name it.
const REASSIGNED_TURNS: u64 = 8;

/// The schedule's reassignments for the start of `turn`, read at the latest block before the start
/// connects, of the turns near it: through `turn` for a member's handover, leaving out the assignment that
/// gave the turn to one of `own`, its member, the latest of the turn; through the turn its promotion
/// carried into for a promoted start, leaving out its own assignment `(turn, position)`.
/// Error cases:
/// - the schedule cannot be read
pub(crate) async fn read_reassigned(
    vault: &impl CheckpointChain,
    turn: u64,
    promoted: Option<(u16, u64)>,
    own: &BTreeSet<AccountId>,
) -> anyhow::Result<Reassigned> {
    let from = turn.saturating_sub(REASSIGNED_TURNS);
    let through = promoted.map_or(turn, |(_, carried)| carried).saturating_add(1);
    let mut assigned = vault.reassignments(from..through).await.context("reading the schedule's reassignments")?;
    match promoted {
        Some((position, _)) => assigned.retain(|at| (at.turn, at.position) != (turn, position)),
        None => {
            let latest = assigned.iter().filter(|at| at.turn == turn).max_by_key(|at| at.position).copied();
            if let Some(latest) = latest.filter(|latest| own.contains(&latest.candidate)) {
                assigned.retain(|at| (at.turn, at.position) != (latest.turn, latest.position));
            }
        }
    }
    Ok(Reassigned { assigned })
}

/// `after_start` for `log`, with the schedule's reassignments its start read (`start_reads`).
/// Error cases:
/// - see `start_reads` and `after_start`
pub(crate) async fn start(
    cfg: &NodeConfig,
    store: Option<&KeyStore>,
    log: &ReplayedLog,
    (promoted, started): (Option<u64>, Option<&Reassigned>),
) -> anyhow::Result<()> {
    let read = start_reads(cfg, store, log, started).await?;
    after_start(store, cfg.operator()?.address, log, promoted, read.as_ref())
}

/// The reassignments `log`'s start counts its own check against: those its member's start read before it
/// connected (`ready`), and for a start that read none, as a whole-log take or a restart, those of the
/// turns through the one in effect, read now while a rotation's turn is in effect and sealed keys take
/// part. The start reads the chain anyway; a failed read fails it.
/// Error cases:
/// - the vault cannot be built, or the schedule cannot be read
pub(crate) async fn start_reads(
    cfg: &NodeConfig,
    store: Option<&KeyStore>,
    log: &ReplayedLog,
    ready: Option<&Reassigned>,
) -> anyhow::Result<Option<Reassigned>> {
    match (ready, store) {
        (Some(read), _) => Ok(Some(read.clone())),
        (None, Some(_)) => match crate::handover::vault_reader(cfg)? {
            Some(vault) => {
                read_unreadied(&vault, log, &own_candidates(&log.replayed.engine, cfg.operator()?.address)).await
            }
            None => Ok(None),
        },
        (None, None) => Ok(None),
    }
}

/// The reassignments of the turns through the one `log` has in effect, while the gate applies.
/// Error cases:
/// - the schedule cannot be read
pub(crate) async fn read_unreadied(
    vault: &impl CheckpointChain,
    log: &ReplayedLog,
    own: &BTreeSet<AccountId>,
) -> anyhow::Result<Option<Reassigned>> {
    let engine = &log.replayed.engine;
    match engine.daily_turns().in_effect.filter(|_| applies(engine)) {
        Some(turn) => read_reassigned(vault, turn.turn, None, own).await.map(Some),
        None => Ok(None),
    }
}

/// The checks a start records: after a handover, the handed-over checkpoint (`after_handover`); for a
/// start a promotion made, the vault's checkpoint at `promoted`, the seq the promotion fixed, which the
/// member's replica verified as it followed (`after_promotion`). Both attribute the range from the log's
/// records and `read`, the schedule's reassignments the start read before it connected (`start_reads`):
/// a promotion or skip of this node's member within the finality window before the start, which the log
/// has yet to mirror, shows only there. A member's start reads no chain between the incumbent's stop and
/// its first command, so a tail take reads them before it connects, and a start with none reads them
/// itself.
/// Error cases:
/// - see `after_handover` and `after_promotion`
pub(crate) fn after_start(
    store: Option<&KeyStore>,
    operator: AccountId,
    log: &ReplayedLog,
    promoted: Option<u64>,
    read: Option<&Reassigned>,
) -> anyhow::Result<()> {
    after_handover(store, operator, log, read)?;
    match promoted {
        Some(checkpoint_seq) => after_promotion(store, operator, log, checkpoint_seq, read),
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
    read: Option<&Reassigned>,
) -> anyhow::Result<()> {
    let replayed = &log.replayed;
    let Some(store) = store.filter(|_| applies(&replayed.engine)) else { return Ok(()) };
    let link = replayed.prefix_ring.prefix_at(checkpoint_seq);
    let (Some(link), Some(root)) = (link, replayed.checkpoint_root(checkpoint_seq)) else {
        return Ok(());
    };
    let head = VerifiedHead { seq: checkpoint_seq, link, root };
    let (release, candidates) = (B256::from(store.release().id()), own_candidates(&replayed.engine, operator));
    let keys = store.keys();
    let previous = keys.log_check;
    let checked = own_check_read(previous, &replayed.takings, &keys.holders, head, (release, &candidates), read);
    if checked == previous {
        return Ok(());
    }
    info!(seq = checkpoint_seq, "the promotion's check: this start verified the log through the vault's checkpoint");
    store.update(|keys| seal_verified(keys, checked)).context("sealing the promotion's verified head")
}

/// Refuses the first start after a handover unless `log` ends at the predecessor's final checkpoint
/// (`keys::check_predecessor_head`), and seals, in the same write, the verified head after it: the
/// handed-over checkpoint, which `log`'s replay or replica verified, unless this release, or the
/// member its node runs for under the `operator` key, sequenced part of the range (`own_check`), as
/// the same member's upgrade did; then the head the keys carried stays. While the gate does not
/// apply the keys carry none, so a release built before the log check can still take them.
/// Error cases:
/// - see `keys::check_predecessor_head`
pub(crate) fn after_handover(
    store: Option<&KeyStore>,
    operator: AccountId,
    log: &ReplayedLog,
    read: Option<&Reassigned>,
) -> anyhow::Result<()> {
    let release = store.map(|store| B256::from(store.release().id()));
    let candidates = own_candidates(&log.replayed.engine, operator);
    let gated = applies(&log.replayed.engine);
    let check = |previous: Option<VerifiedHead>, holders: &[Holder], head: &Head| {
        if !gated {
            return None;
        }
        let (Some(release), Ok(root)) = (release, head.root.parse()) else { return previous };
        let handed = VerifiedHead { seq: head.seq, link: log.head_link(), root };
        let checked = own_check_read(previous, &log.replayed.takings, holders, handed, (release, &candidates), read);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoints::{Assigned, VaultState, View};

    /// A schedule that answers the reassignments of turn 7, and only those, and records the range asked.
    struct Schedule(std::sync::Mutex<Option<std::ops::Range<u64>>>);

    impl CheckpointChain for Schedule {
        async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
            anyhow::bail!("this chain reads no vault state")
        }

        async fn post(&self, _checkpoint: &crate::log::Checkpoint) -> anyhow::Result<()> {
            anyhow::bail!("this chain posts nothing")
        }

        async fn reassignments(&self, turns: std::ops::Range<u64>) -> anyhow::Result<Vec<Assigned>> {
            *self.0.lock().unwrap() = Some(turns);
            let at = |position, reassigned| Assigned { turn: 7, position, candidate: AccountId([9; 20]), reassigned };
            Ok(vec![at(0, false), at(1, true), at(2, true)])
        }
    }

    /// A handover start reads the turns through its own, and a promoted start those through the turn it
    /// carries into, leaving out its own promotion; a schedule that cannot be read fails the read.
    #[tokio::test]
    async fn a_start_reads_the_reassignments_of_the_turns_its_check_may_span() {
        let schedule = Schedule(Default::default());
        let handover = read_reassigned(&schedule, 20, None, &BTreeSet::new()).await.unwrap();
        assert_eq!(handover.assigned.len(), 3);
        assert_eq!(schedule.0.lock().unwrap().clone(), Some(12..21), "through the turn taken");
        let own = BTreeSet::from([AccountId([9; 20])]);
        let taken = read_reassigned(&schedule, 7, None, &own).await.unwrap();
        assert_eq!(
            taken.assigned.iter().map(|at| at.position).collect::<Vec<_>>(),
            [0, 1],
            "the assignment that gave it"
        );
        let promoted = read_reassigned(&schedule, 7, Some((2, 8)), &BTreeSet::new()).await.unwrap();
        assert_eq!(schedule.0.lock().unwrap().clone(), Some(0..9));
        assert_eq!(promoted.assigned.iter().map(|at| at.position).collect::<Vec<_>>(), [0, 1], "its own promotion");
        struct Unread;
        impl CheckpointChain for Unread {
            async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
                anyhow::bail!("unread")
            }

            async fn post(&self, _checkpoint: &crate::log::Checkpoint) -> anyhow::Result<()> {
                anyhow::bail!("unread")
            }
        }
        assert!(read_reassigned(&Unread, 20, None, &BTreeSet::new()).await.is_err(), "a failed read dropped the check");
    }
}
