//! A node's part in the boundary's log check (`log_check`): the check its first start after a
//! handover records, the sequencer's setup, and the asking of a node that takes turns.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Context;
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::checkpoints::CheckpointChain;
use crate::config::NodeConfig;
use crate::log::CheckpointDomain;
use crate::log_check::ask::Asker;
use crate::log_check::{Due, Takings, VerifiedHead, applies, own_candidates, own_check};
use crate::node::ready::Ready;
use crate::node::shutdown::TaskResult;
use crate::sequencer::{LogCheckSetup, ReplayedLog, Request};
use crate::tee::EnclaveKey;
use crate::tee::keys::Head;
use crate::tee::sealing::KeyStore;
use crate::trusted_time::TrustedClock;

/// The checks a start records, with `cfg`'s operator key: after a handover, the handed-over checkpoint
/// (`after_handover`); for a start a promotion made, the vault's checkpoint the promotion fixed, which
/// the member's replica verified as it followed (`after_promotion`). Each counts only when the schedule
/// promoted or skipped to the member its node runs for in no turn from the range's first up to the one
/// `ready` takes (`held`). A failure raises Critical `log_recovery_failed`.
/// Error cases:
/// - the vault's reader cannot be built, or see `after_handover` and `after_promotion`
pub(crate) async fn after_start(
    cfg: &NodeConfig,
    store: Option<&KeyStore>,
    log: &ReplayedLog,
    ready: Option<&Ready>,
) -> anyhow::Result<()> {
    let checked = async {
        let operator = cfg.operator()?.address;
        let vault = crate::handover::vault_reader(cfg)?;
        let taking = ready.and_then(|ready| ready.generations.keys().last().copied());
        let (candidates, gated) = (own_candidates(&log.replayed.engine, operator), applies(&log.replayed.engine));
        let held = async |through| {
            let previous = store.and_then(|store| store.keys().log_check).map(|head| head.seq);
            gated && held(vault.as_ref(), &log.replayed.takings, (previous, through), taking, &candidates).await
        };
        after_handover(store, operator, log, held(log.last_seq().unwrap_or(0)).await)?;
        match ready.and_then(|ready| ready.promoted) {
            Some(promotion) => {
                let seq = promotion.checkpoint_seq;
                after_promotion(store, operator, log, seq, held(seq).await)
            }
            None => Ok(()),
        }
    };
    checked.await.inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))
}

/// Whether the schedule `vault` reads promoted, or skipped to, any of `candidates`, the members a node
/// runs for, in a turn from the first of the range after `previous` through `through` up to `taking`,
/// the turn this start takes, so that this node's own verification of the range counts for nothing:
/// such a member sequences before the log mirrors its assignment. True when the start takes no known turn, the
/// records cannot attribute the range, or the chain cannot say.
async fn held(
    vault: Option<&impl CheckpointChain>,
    takings: &Takings,
    (previous, through): (Option<u64>, u64),
    taking: Option<u64>,
    candidates: &BTreeSet<AccountId>,
) -> bool {
    let first = takings.sequenced(previous, through).map(|sequencers| sequencers.turns_from);
    let (Some(vault), Some(taking), Some(first)) = (vault, taking, first) else { return true };
    match vault.reassigned(first..taking).await {
        Ok(reassigned) => !reassigned.is_disjoint(candidates),
        Err(e) => {
            warn!("reading whom the schedule promoted or skipped to in the range's turns failed: {e:#}");
            true
        }
    }
}

/// Seals, for a start a promotion made, the verified head after the member's own verification of the
/// log through `checkpoint_seq`, the vault's checkpoint the promotion fixed, which its replica checked
/// entry by entry as it followed: counted, as a successor's at a handover, only while the gate applies,
/// the schedule promoted or skipped to the member in no turn of the range before this one (`held`),
/// and neither this
/// release nor the member its node runs for under the `operator` key sequenced part of the range
/// (`own_check`); otherwise the head the keys carry stays, and a following member's check is asked for
/// once this release has taken the turn. A log that no longer holds that checkpoint's link or root, as
/// a restart long past it, records nothing.
/// Error cases:
/// - the keys cannot be sealed again
pub(crate) fn after_promotion(
    store: Option<&KeyStore>,
    operator: AccountId,
    log: &ReplayedLog,
    checkpoint_seq: u64,
    held: bool,
) -> anyhow::Result<()> {
    let replayed = &log.replayed;
    let Some(store) = store.filter(|_| applies(&replayed.engine) && !held) else { return Ok(()) };
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
/// the same member's upgrade did, or the schedule promoted or skipped to that member in a turn of the
/// range (`held`);
/// then the head the keys carried stays. While the gate does not apply the keys carry none, so a
/// release built before the log check can still take them.
/// Error cases:
/// - see `keys::check_predecessor_head`
pub(crate) fn after_handover(
    store: Option<&KeyStore>,
    operator: AccountId,
    log: &ReplayedLog,
    held: bool,
) -> anyhow::Result<()> {
    let release = store.map(|store| B256::from(store.release().id()));
    let candidates = own_candidates(&log.replayed.engine, operator);
    let gated = applies(&log.replayed.engine);
    let check = |previous: Option<VerifiedHead>, head: &Head| {
        if !gated {
            return None;
        }
        if held {
            return previous;
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use engine_core::event::Event;

    use super::*;
    use crate::checkpoints::{VaultState, View};
    use crate::log::Checkpoint;

    /// A schedule that promoted or skipped to `reassigned` in the turns asked about, or cannot be read,
    /// and the turns it was last asked about.
    struct Schedule {
        reassigned: Option<BTreeSet<AccountId>>,
        asked: Mutex<Option<std::ops::Range<u64>>>,
    }

    impl CheckpointChain for Schedule {
        async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
            anyhow::bail!("this schedule reads no vault")
        }

        async fn post(&self, _checkpoint: &Checkpoint) -> anyhow::Result<()> {
            anyhow::bail!("this schedule takes no checkpoints")
        }

        async fn reassigned(&self, turns: std::ops::Range<u64>) -> anyhow::Result<BTreeSet<AccountId>> {
            *self.asked.lock().unwrap() = Some(turns);
            self.reassigned.clone().context("the chain cannot be read")
        }
    }

    /// A start's own check of a range counts for nothing when the schedule promoted or skipped to its
    /// member in a turn from the one in office at the range's start up to the one the start takes, when
    /// the chain cannot say, when the start takes no known turn, and when no record attributes the range.
    #[tokio::test]
    async fn a_start_holds_its_check_when_the_schedule_reassigned_its_member_in_the_ranges_turns() {
        let (own, other) = (AccountId([0xb0; 20]), AccountId([0xc0; 20]));
        let mut takings = Takings::default();
        let taken = Event::TurnTaken { turn: 2, candidate: AccountId([0xa0; 20]), release: B256::repeat_byte(0xa1) };
        takings.command(10, &[taken]);
        let (mine, range) = (BTreeSet::from([own]), (Some(20), 30));
        for (case, reassigned, taking, held_back) in [
            ("its member", Some(BTreeSet::from([own])), Some(5), true),
            ("another member", Some(BTreeSet::from([other])), Some(5), false),
            ("an unread chain", None, Some(5), true),
            ("no known turn", Some(BTreeSet::new()), None, true),
        ] {
            let schedule = Schedule { reassigned, asked: Mutex::new(None) };
            assert_eq!(held(Some(&schedule), &takings, range, taking, &mine).await, held_back, "{case}");
            let asked = taking.map(|_| 2..5);
            assert_eq!(*schedule.asked.lock().unwrap(), asked, "{case}: the turns from the one in office");
        }
        let schedule = Schedule { reassigned: Some(BTreeSet::new()), asked: Mutex::new(None) };
        assert!(held(Some(&schedule), &Takings::default(), range, Some(5), &mine).await, "an unattributed range");
    }
}
