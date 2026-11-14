//! A node's part in the boundary's log check (`log_check`): the check its first start after a
//! handover records, the sequencer's setup, and the asking of a node that takes turns.

use std::sync::Arc;

use engine_core::eip712::B256;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::info;

use crate::config::NodeConfig;
use crate::log::CheckpointDomain;
use crate::log_check::ask::Asker;
use crate::log_check::{Due, Takings, VerifiedHead, own_check};
use crate::node::KeyStart;
use crate::node::shutdown::TaskResult;
use crate::sequencer::{LogCheckSetup, ReplayedLog, Request};
use crate::tee::keys::Head;
use crate::tee::sealing::KeyStore;
use crate::trusted_time::TrustedClock;

/// Refuses the first start after a handover unless `log` ends at the predecessor's final checkpoint
/// (`keys::check_predecessor_head`), and seals, in the same write, the verified head after it: the
/// handed-over checkpoint, which `log`'s replay or replica verified, unless the operator's
/// `--predecessor` in `start` brought the keys, an upgrade of the same member, which carries on the
/// head the keys carried, or this release sequenced part of the range (`own_check`).
/// Error cases:
/// - see `keys::check_predecessor_head`
pub(crate) fn after_handover(store: Option<&KeyStore>, start: &KeyStart, log: &ReplayedLog) -> anyhow::Result<()> {
    let release = store.map(|store| B256::from(store.release().id()));
    let check = |previous: Option<VerifiedHead>, head: &Head| {
        let (Some(release), Ok(root)) = (release, head.root.parse()) else { return previous };
        let handed = VerifiedHead { seq: head.seq, link: log.head_link(), root };
        let upgrade = start.predecessor.is_some();
        let checked = handed_over(upgrade, previous, &log.replayed.takings, handed, release);
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

/// The verified head after a handover of the keys and the log through `handed` to `release`, from
/// the `previous` one the keys carried: an `upgrade` carries `previous` on, and any other handover
/// takes `handed` when `release` sequenced none of the range since `previous` (`own_check`).
fn handed_over(
    upgrade: bool,
    previous: Option<VerifiedHead>,
    takings: &Takings,
    handed: VerifiedHead,
    release: B256,
) -> Option<VerifiedHead> {
    if upgrade { previous } else { own_check(previous, takings, handed, release) }
}

/// The sequencer's log check setup for a node whose sealed `store` takes turns, and the receiving end
/// of its checks due; neither without one.
pub(super) fn setup(store: Option<&Arc<KeyStore>>) -> (Option<LogCheckSetup>, Option<watch::Receiver<Option<Due>>>) {
    let Some(store) = store else { return (None, None) };
    let (due, asking) = watch::channel(None);
    let release = B256::from(store.release().id());
    (Some(LogCheckSetup { release, store: store.clone(), due }), Some(asking))
}

/// Spawns the asking for the checks `due` publishes, reading the vault `[checkpoints]` names and
/// verifying quotes against `store`'s platform at `trusted` time.
/// Error cases:
/// - the vault's client cannot be built, or the vault address does not parse
pub(super) fn spawn(
    cfg: &NodeConfig,
    tasks: &mut JoinSet<TaskResult>,
    (due, store): (Option<watch::Receiver<Option<Due>>>, Option<&Arc<KeyStore>>),
    sequencer: &mpsc::Sender<Request>,
    trusted: TrustedClock,
) -> anyhow::Result<()> {
    let (Some(due), Some(store), Some(vault)) = (due, store, crate::handover::vault_reader(cfg)?) else {
        return Ok(());
    };
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let asker = Asker::new(vault, sequencer, due, (domain, cfg.tee.provider, store.platform()), trusted);
    tasks.spawn(async move { ("log check", asker.run().await) });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use engine_core::event::Event;
    use engine_core::types::AccountId;

    const PREDECESSOR: B256 = B256::repeat_byte(0xa1);
    const SUCCESSOR: B256 = B256::repeat_byte(0xb1);

    #[test]
    fn a_turns_handover_is_the_successors_check_and_an_upgrade_carries_the_predecessors() {
        let mut takings = Takings::default();
        takings.command(0, &[], None);
        let taken = Event::TurnTaken { turn: 1, candidate: AccountId([1; 20]), release: PREDECESSOR };
        takings.command(10, &[taken], Some(PREDECESSOR));
        let (previous, handed) = (Some(head(9)), head(40));
        assert_eq!(handed_over(false, previous, &takings, handed, SUCCESSOR), Some(handed));
        assert_eq!(handed_over(true, previous, &takings, handed, SUCCESSOR), previous, "an upgrade checked itself");
        assert_eq!(
            handed_over(false, previous, &takings, handed, PREDECESSOR),
            previous,
            "a release in the range checked"
        );
    }

    fn head(seq: u64) -> VerifiedHead {
        VerifiedHead { seq, link: B256::repeat_byte(seq as u8), root: B256::repeat_byte(!(seq as u8)) }
    }
}
