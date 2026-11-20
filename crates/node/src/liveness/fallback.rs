//! A member's fallback fixer, for the node process's whole life, while it follows and while it holds a
//! turn: from the liveness caller's account (`[liveness] caller`, signed by the `[checkpoints]`
//! endpoint), it fixes the fallback of the turn whose draw deadline last passed when no seed
//! was relayed for it (`SeedReceiver.fixFallback`), while the registry's rotation switch is on, or, once
//! anyone has fixed its block, whatever the switch, since the switch may come back on before the turn's
//! boundary. One
//! call records the block it lands in, and any later one, from a later block, records that block's hash,
//! well within the 256 blocks `ArbSys` answers for. So a member that sees the hash cannot keep the turn
//! from being drawn by leaving it unrecorded: not one taking a turn at the boundary, while the holder
//! still sequences, either. Arbitrum's sequencer, which orders every call, still can (`docs/security-model.md`).
//!
//! It reads every `[liveness] fallback_interval` while that turn has a relayed seed or a recorded
//! fallback, and once more right after each draw deadline; every `WATCH` while it has neither; and every
//! `FOLLOW_UP` once a fix is sent or a block is fixed, by anyone, until its hash is recorded. So a read
//! always comes well within the 256 blocks (64 s on Arbitrum One) after anyone's first fix, a draw's
//! included.
//!
//! It reads through a custody reader of its own, which holds a read session for each pinned block, so
//! no other read waits on it or delays it, and it sends one call at a time with the member's liveness
//! caller (`Sends`). Each poll it also reads the account's balance, raising `liveness_caller_unfunded`
//! below `[liveness] min_balance` in one episode with the caller's, so the account is watched while the
//! member holds a turn too. It pages `liveness_caller_fallback_failed` while it cannot read or send,
//! once per episode, and `seed_fallback_lapsed` once for each turn whose fixed block has passed that
//! reach unrecorded, until the turn's boundary; its first read resolves a lapse an earlier run left
//! open.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;
use anyhow::Context as _;
use engine_core::types::AccountId;
use futures_util::future::BoxFuture;
use tracing::{debug, info, warn};

use crate::config::NodeConfig;
use crate::custody::StandbyReader;
use crate::liveness::caller::{Endpoint, FOLLOW_UP, Sending, check_balance};
use crate::liveness::chain::{PendingFallback, fixFallbackCall, pending_fallback};
use crate::seed_relay::schedule::{TURN_SECS, draw_deadline, last_final_turn};
use crate::standby_key::chain::{AgreedBlock, StandbyChain, StandbyDomain};

/// How many blocks back `ArbSys.arbBlockHash` answers: past it a fixed block's hash is never recorded.
const BLOCK_HASH_REACH: u64 = 256;
/// The fewest reads a turn the fixer makes while idle, so the development build's five-minute turns
/// keep it reading too.
const IDLE_READS_PER_TURN: u64 = 8;
/// How often it reads while the turn has neither a relayed seed nor a recorded fallback and no block is
/// fixed for it: anyone's first fix then lands at most this long, plus a read's time, before a read sees
/// it, well within the 256 blocks its hash can be recorded for.
const WATCH: Duration = Duration::from_secs(20);

/// The liveness caller's account, which the member's liveness caller and its fixer both send from.
#[derive(Debug, Default)]
pub(crate) struct Account {
    /// Whether `liveness_caller_unfunded` is raised, so the two raise and resolve it as one episode.
    pub unfunded: bool,
}

/// Held for each send from the liveness caller's account and each read of its balance, so its fixer and
/// its liveness calls send one call at a time.
pub(crate) type Sends = tokio::sync::Mutex<Account>;

/// What a poll found for the turn whose draw deadline last passed, at a block of time `time`.
struct Found {
    time: u64,
    fallback: Fallback,
}

/// That turn's fallback.
enum Fallback {
    /// A relayed seed or a recorded fallback, or no schedule: nothing to fix.
    Settled,
    /// Neither, with the rotation switch off and no block fixed: nothing to send yet.
    Waiting,
    /// The fix sent: its transaction hash, `None` when the chain would refuse it, or the send's failure.
    Sent(PendingFallback, anyhow::Result<Option<B256>>),
    /// Its fixed block has passed the reach unrecorded.
    Lapsed(PendingFallback),
}

/// A member's fallback fixer.
pub(crate) struct Fixer<C, S> {
    /// A reader of its own.
    pub chain: C,
    pub sending: S,
    /// The custody chain and its vault, whose registry and schedule it reads at each block.
    pub chain_id: u64,
    pub vault: Address,
    /// How often it reads while the turn's fallback is settled: `[liveness] fallback_interval`, and at
    /// least `IDLE_READS_PER_TURN` times a turn.
    pub idle: Duration,
    /// How soon it reads again once a fix is under way: `FOLLOW_UP`; at most `WATCH`.
    pub poll: Duration,
    /// Below this many wei, `liveness_caller_unfunded` fires.
    pub min_balance: U256,
    pub sends: Arc<Sends>,
}

impl Fixer<StandbyReader, Endpoint> {
    /// The fixer `cfg` runs, sharing `sends` with the member's liveness caller: with `[liveness]`,
    /// `[checkpoints]`, `[log_heads]` and EVM custody; `None` otherwise.
    /// Error cases:
    /// - the custody reader or the endpoint cannot be built, or `custody.vault` does not parse
    pub(crate) fn for_config(cfg: &NodeConfig, sends: Arc<Sends>) -> anyhow::Result<Option<Self>> {
        let (Some(liveness), Some(checkpoints), Some(log_heads)) = (&cfg.liveness, &cfg.checkpoints, &cfg.log_heads)
        else {
            return Ok(None);
        };
        if cfg.custody.provider != crate::config::CustodyProvider::Evm {
            return Ok(None);
        }
        Ok(Some(Self {
            chain: StandbyReader::connect_named(&cfg.custody, log_heads.contract, "fallback_fixer")?,
            sending: Endpoint::new(checkpoints, liveness, cfg.custody.chain_id)?,
            chain_id: cfg.custody.chain_id,
            vault: Address::from(cfg.custody.vault.parse::<AccountId>().context("custody.vault")?.0),
            idle: liveness.fallback_interval,
            poll: FOLLOW_UP,
            min_balance: liveness.min_balance,
            sends,
        }))
    }
}

impl<C: StandbyChain, S: Sending> Fixer<C, S> {
    /// Runs the fixer until its task is dropped.
    /// Error cases:
    /// - none: failed reads and sends are retried on the next poll
    pub(crate) async fn run(self) -> anyhow::Result<()> {
        let idle = self.idle.min(Duration::from_secs(TURN_SECS / IDLE_READS_PER_TURN));
        // The lapsed turn, if any, as of the last read; `None` before the first.
        let (mut failed, mut lapsed) = (false, None);
        // Until a read succeeds, as while it does not know the turn has a seed.
        let mut pause = WATCH.min(idle);
        loop {
            let found: BoxFuture<'_, anyhow::Result<Found>> = Box::pin(self.fix());
            let found = found.await;
            let funded: BoxFuture<'_, anyhow::Result<()>> =
                Box::pin(async { check_balance(&self.sending, self.min_balance, &mut *self.sends.lock().await).await });
            let failure = match (&found, funded.await) {
                (Err(e), _) => Some(format!("cannot fix turns: {e:#}")),
                (Ok(Found { fallback: Fallback::Sent(_, Err(e)), .. }), _) => Some(format!("cannot fix turns: {e:#}")),
                (Ok(_), Err(e)) => Some(format!("cannot read the liveness caller's balance: {e:#}")),
                (Ok(_), Ok(())) => None,
            };
            match failure {
                Some(e) if !std::mem::replace(&mut failed, true) => {
                    warn!(alert = "liveness_caller_fallback_failed", "the fallback fixer {e}");
                }
                None if std::mem::take(&mut failed) => info!(
                    alert_resolved = "liveness_caller_fallback_failed",
                    "the fallback fixer reads the seed receiver and the caller's balance and sends its fixes again"
                ),
                _ => {}
            }
            // A failed read keeps its cadence.
            if let Ok(found) = found {
                pause = self.pause(&found, idle);
                report(found.fallback, &mut lapsed);
            }
            tokio::time::sleep(pause).await;
        }
    }

    /// How long to wait after a poll that found `found`: `FOLLOW_UP` once a fix is sent or a block is
    /// fixed, `WATCH` while the turn has neither a seed nor a fallback, and otherwise `idle`, ending just
    /// past the next draw deadline; never longer than `idle`.
    fn pause(&self, found: &Found, idle: Duration) -> Duration {
        match &found.fallback {
            Fallback::Sent(pending, sent) if pending.fixed_in != 0 || matches!(sent, Ok(Some(_))) => {
                self.poll.min(WATCH)
            }
            Fallback::Sent(..) | Fallback::Waiting => WATCH.min(idle),
            Fallback::Settled | Fallback::Lapsed(_) => {
                let deadline = draw_deadline(last_final_turn(found.time).saturating_add(1));
                let until = deadline.map(|deadline| Duration::from_secs(deadline.saturating_sub(found.time) + 1));
                until.map_or(idle, |until| idle.min(until))
            }
        }
    }

    /// One poll: the turn whose draw deadline last passed at the latest block, and the fix sent for its
    /// fallback while the rotation switch is on or a block is fixed for it, unless that block has passed
    /// the reach.
    /// Error cases:
    /// - the read fails; a failed send comes back with the pending fallback
    async fn fix(&self) -> anyhow::Result<Found> {
        let block = self.chain.latest().await?;
        let time = block.time();
        let Some(domain) = StandbyDomain::at_if_scheduled(&block, self.chain_id, self.vault).await? else {
            return Ok(Found { time, fallback: Fallback::Settled });
        };
        let Some(pending) = pending_fallback(&block, &domain).await? else {
            return Ok(Found { time, fallback: Fallback::Settled });
        };
        if !pending.drawable {
            return Ok(Found { time, fallback: Fallback::Waiting });
        }
        if pending.fixed_in != 0 && block.number().saturating_sub(pending.fixed_in) > BLOCK_HASH_REACH {
            return Ok(Found { time, fallback: Fallback::Lapsed(pending) });
        }
        drop(block);
        let _one = self.sends.lock().await;
        let data = fixFallbackCall { turn: pending.turn }.abi_encode();
        Ok(Found { time, fallback: Fallback::Sent(pending, self.sending.send(pending.receiver, data).await) })
    }
}

/// Logs a poll's outcome. `seed_fallback_lapsed` is raised once for a turn whose fixed block has passed
/// the reach unrecorded and resolved once a read finds that turn no longer pending, at its boundary,
/// when the next turn's draw deadline passes; `lapsed` holds the lapsed turn as of the last read, `None`
/// before the first, which resolves an episode an earlier run left open unless a turn has lapsed.
fn report(fallback: Fallback, lapsed: &mut Option<Option<u64>>) {
    let now = match &fallback {
        Fallback::Lapsed(pending) => Some(pending.turn),
        _ => None,
    };
    if *lapsed != Some(now) {
        // The episode's key carries no turn, so a restart can resolve whichever turn it left open.
        if matches!((*lapsed, now), (None, None) | (Some(Some(_)), _)) {
            info!(alert_resolved = "seed_fallback_lapsed", "no turn's fixed fallback block has lapsed unrecorded");
        }
        if let Fallback::Lapsed(pending) = &fallback {
            warn!(
                alert = "seed_fallback_lapsed",
                "turn {}'s fallback block {} has passed the {BLOCK_HASH_REACH} blocks its hash is readable for \
                 unrecorded: the turn can no longer be drawn, and the member before it stays named through it",
                pending.turn,
                pending.fixed_in
            );
        }
        *lapsed = Some(now);
    }
    match fallback {
        Fallback::Sent(pending, Ok(Some(hash))) => info!(turn = pending.turn, %hash, "fallback fix sent"),
        Fallback::Sent(pending, Ok(None)) => {
            debug!(turn = pending.turn, "the chain would refuse the fallback fix: not sent");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
