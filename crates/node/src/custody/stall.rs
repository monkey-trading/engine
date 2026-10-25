//! What a poll of the vault did: read a range, waited for finality, or stopped short, and the
//! alert each kind of stall raises.

use tokio::sync::mpsc;
use tokio::time::{Instant, sleep};
use tracing::{Instrument, debug, error, info, info_span, warn};

use crate::custody::evm::{DepositWatcher, VaultReader};
use crate::sequencer::Request;

/// What a poll that did not stop short did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Polled {
    /// It read a block range; `caught_up` when that range reached the final block.
    Read { caught_up: bool },
    /// No final block has reached the cursor yet.
    Waiting,
}

/// What a refused stamp is logged under: trusted time alerts it, so it raises no alert of its own.
const REFUSED: &str = "custody_stamp_refused";

/// An uninterrupted run of stalled polls.
struct StallEpisode {
    /// The alerts raised so far, so each kind of stall alerts once.
    raised: Vec<&'static str>,
    since: Instant,
    escalated: bool,
}

/// Why a poll stopped short.
#[derive(Debug)]
pub(super) enum PollError {
    /// The next poll retries.
    Stalled(Stall),
    /// The sequencer or the cursor file failed, so the node must stop.
    Fatal(anyhow::Error),
}

/// A poll that read nothing it could act on. Nothing is credited or skipped until it clears.
#[derive(Debug)]
pub(super) enum Stall {
    /// The chain could not be read.
    Rpc(anyhow::Error),
    /// The chain's answers disagree in a way a lagging backend explains, so a retry may settle it.
    Unsettled(anyhow::Error),
    /// The chain contradicts the configuration or the cursor; no retry is likely to fix it.
    Inconsistent(anyhow::Error),
    /// The sequencer refused a deposit's stamp, as it does until trusted time admits one, which
    /// trusted time alerts itself.
    Refused(anyhow::Error),
}

impl PollError {
    /// A chain read that failed.
    pub(super) fn rpc(e: anyhow::Error) -> Self {
        PollError::Stalled(Stall::Rpc(e))
    }

    /// Chain answers that a lagging backend could explain.
    pub(super) fn unsettled(e: anyhow::Error) -> Self {
        PollError::Stalled(Stall::Unsettled(e))
    }

    /// Chain answers that contradict the configuration or the cursor.
    pub(super) fn inconsistent(e: anyhow::Error) -> Self {
        PollError::Stalled(Stall::Inconsistent(e))
    }
}

impl Stall {
    /// What stopped the poll.
    pub(super) fn error(&self) -> &anyhow::Error {
        match self {
            Stall::Rpc(e) | Stall::Unsettled(e) | Stall::Inconsistent(e) | Stall::Refused(e) => e,
        }
    }

    /// The alert this stall raises, or for a refused stamp the name it is logged under, so a
    /// repeat of the same stall stays quiet.
    pub(super) fn alert(&self) -> &'static str {
        match self {
            Stall::Rpc(_) => "custody_rpc_failed",
            Stall::Unsettled(_) => "custody_chain_unsettled",
            Stall::Inconsistent(_) => "custody_chain_inconsistent",
            Stall::Refused(_) => REFUSED,
        }
    }

    /// Raises the alert: a Warning when a retry may clear it (a stall that lasts escalates
    /// separately), Critical when it contradicts the configuration or the cursor. A refused stamp
    /// only logs, since trusted time alerts it. The block goes in the message, so the alert's key
    /// is its kind alone.
    pub(super) fn raise(&self, next_block: u64) {
        match self {
            Stall::Rpc(e) | Stall::Unsettled(e) => {
                warn!(alert = self.alert(), "deposits paused at block {next_block}: {e:#}")
            }
            Stall::Inconsistent(e) => error!(alert = self.alert(), "deposits paused at block {next_block}: {e:#}"),
            Stall::Refused(e) => info!(next_block, "deposits wait for trusted time: {e:#}"),
        }
    }
}

impl StallEpisode {
    /// Resolves each alert the episode raised, now that a read has gone through.
    fn resolve(&self) {
        let escalated = self.escalated.then_some("custody_deposits_stalled");
        for alert in self.raised.iter().copied().filter(|alert| *alert != REFUSED).chain(escalated) {
            info!(alert_resolved = alert, "vault deposits are read again");
        }
    }
}

impl Polled {
    /// Whether the watcher has nothing left to read: it reached the final block, or waits on it.
    #[cfg(test)]
    pub(super) fn caught_up(self) -> bool {
        match self {
            Polled::Read { caught_up } => caught_up,
            Polled::Waiting => true,
        }
    }
}

impl<V: VaultReader> DepositWatcher<V> {
    /// Polls until a fatal error. A stalled poll raises its alert once per kind of stall in an
    /// episode and is retried every poll interval; a stall that outlasts `stall_critical_after`
    /// raises a Critical alert once.
    /// Error cases:
    /// - the sequencer has stopped, or the cursor cannot be stored
    pub async fn run(mut self, tx: mpsc::Sender<Request>) -> anyhow::Result<()> {
        let mut stalled: Option<StallEpisode> = None;
        loop {
            let next_block = self.cursor.next_block;
            let caught_up = match self.poll(&tx).instrument(info_span!("custody_poll", id = next_block)).await {
                Ok(Polled::Read { caught_up }) => {
                    if let Some(episode) = stalled.take() {
                        info!(next_block, stalled_for = ?episode.since.elapsed(), "deposit watcher resumed");
                        episode.resolve();
                    }
                    caught_up
                }
                // Waiting reads nothing, so it neither ends a stall nor stops its clock.
                Ok(Polled::Waiting) => true,
                Err(PollError::Fatal(e)) => return Err(e),
                Err(PollError::Stalled(stall)) => {
                    let episode = stalled.get_or_insert(StallEpisode {
                        raised: Vec::new(),
                        since: Instant::now(),
                        escalated: false,
                    });
                    if episode.raised.contains(&stall.alert()) {
                        debug!(next_block, "deposit watcher still stalled: {:#}", stall.error());
                    } else {
                        stall.raise(next_block);
                        episode.raised.push(stall.alert());
                    }
                    let stalled_for = episode.since.elapsed();
                    if !episode.escalated && stalled_for >= self.stall_critical_after {
                        let cause = stall.error();
                        error!(
                            alert = "custody_deposits_stalled",
                            "no vault deposit read at block {next_block} for {stalled_for:?}: {cause:#}"
                        );
                        episode.escalated = true;
                    }
                    true
                }
            };
            if caught_up {
                sleep(self.poll_interval).await;
            }
        }
    }
}
