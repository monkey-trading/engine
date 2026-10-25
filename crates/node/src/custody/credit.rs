//! Sequencing vault deposits: crediting one deposit once, and the round trips to the sequencer
//! it needs.

use std::collections::BTreeSet;

use anyhow::anyhow;
use engine_core::Engine;
use engine_core::command::Command;
use engine_core::event::Event;
use tokio::sync::{mpsc, oneshot};
use tracing::{Instrument, debug, error, info, info_span};

use crate::custody::evm::{Beneficiary, VaultDeposit};
use crate::custody::stall::{PollError, Stall};
use crate::custody::usdc_to_fixed;
use crate::sequencer::Request;

/// Sequences one legacy configured-tag deposit, unless already credited. Its span carries the
/// reference as `id`.
/// Error cases:
/// - stalled: the sequencer refused the command's stamp (see `submit`)
/// - fatal: the sequencer has stopped
pub(super) async fn credit(tx: &mpsc::Sender<Request>, deposit: &VaultDeposit) -> Result<(), PollError> {
    let chain_ref = deposit.chain_ref();
    let span = info_span!("deposit", id = %chain_ref, block = deposit.block, deposit_id = deposit.id);
    async move {
        let Some(amount) = usdc_to_fixed(deposit.units) else {
            // Nothing re-examines it, and the USDC stays in the vault uncredited.
            // Keyed by the deposit, so each one pages; the amount goes in the message.
            error!(
                alert = "deposit_unrepresentable",
                chain_ref = %deposit.chain_ref(),
                "deposit of {} USDC units too large to credit",
                deposit.units
            );
            return Ok(());
        };
        let credited_ref = chain_ref.clone();
        if query(tx, move |engine| engine.is_credited(&credited_ref)).await.map_err(PollError::Fatal)? {
            debug!("deposit already credited");
            return Ok(());
        }
        let cmd = match deposit.beneficiary {
            Beneficiary::Trader(account) => Command::Deposit { account, amount, chain_ref },
            Beneficiary::House => Command::HouseCapital { amount, chain_ref },
        };
        for event in submit(tx, cmd).await? {
            match event {
                Event::Deposited { account, amount } => info!(%account, %amount, "deposit credited"),
                Event::HouseCapitalMoved { amount, nav } => info!(%amount, %nav, "house deposit credited"),
                // The sequencer alerts a refused trader deposit as `deposit_rejected`; a refused
                // house deposit is an admin rejection, which it does not alert.
                Event::AdminRejected { reason } => {
                    error!(
                        alert = "house_deposit_rejected",
                        chain_ref = %deposit.chain_ref(),
                        "house deposit of {amount} not credited: {reason}"
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }
    .instrument(span)
    .await
}

/// References already credited for the transactions in `deposits` at log indices those
/// deposits no longer carry: a transaction the chain moved after it was credited.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn moved_credits(
    tx: &mpsc::Sender<Request>,
    deposits: &[VaultDeposit],
) -> anyhow::Result<Vec<String>> {
    let current: BTreeSet<String> = deposits.iter().map(VaultDeposit::chain_ref).collect();
    let prefixes: BTreeSet<String> = deposits.iter().map(|d| format!("{}:", d.tx_hash)).collect();
    query(tx, move |engine| {
        prefixes
            .iter()
            .flat_map(|prefix| engine.credited_refs_with_prefix(prefix))
            .filter(|chain_ref| !current.contains(*chain_ref))
            .map(str::to_owned)
            .collect()
    })
    .await
}

/// Sequences `cmd` and returns its events once they are durable.
/// Error cases:
/// - stalled: the sequencer dropped the reply but still runs, as it does for a command whose stamp
///   trusted time refuses; nothing was sequenced, and the next poll retries
/// - fatal: the sequencer has stopped
pub(super) async fn submit(tx: &mpsc::Sender<Request>, cmd: Command) -> Result<Vec<Event>, PollError> {
    let stopped = || PollError::Fatal(anyhow!("sequencer stopped"));
    let (reply, rx) = oneshot::channel();
    tx.send(Request::Submit { cmd, reply: Some(reply) }).await.map_err(|_| stopped())?;
    match rx.await {
        Ok(events) => Ok(events),
        Err(_) if !tx.is_closed() => {
            Err(PollError::Stalled(Stall::Refused(anyhow!("the sequencer refused the deposit's stamp"))))
        }
        Err(_) => Err(stopped()),
    }
}

/// Waits until every command sequenced so far is synced to storage, and returns the last seq.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn sync(tx: &mpsc::Sender<Request>) -> anyhow::Result<Option<u64>> {
    let (reply, rx) = oneshot::channel();
    tx.send(Request::Sync(reply)).await.map_err(|_| anyhow!("sequencer stopped"))?;
    rx.await.map_err(|_| anyhow!("sequencer stopped"))
}

/// Reads the engine between sequencer batches.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn query<T: Send + 'static>(
    tx: &mpsc::Sender<Request>,
    read: impl FnOnce(&Engine) -> T + Send + 'static,
) -> anyhow::Result<T> {
    let (reply, rx) = oneshot::channel();
    let q = Box::new(move |engine: &Engine| {
        let _ = reply.send(read(engine));
    });
    tx.send(Request::Query(q)).await.map_err(|_| anyhow!("sequencer stopped"))?;
    rx.await.map_err(|_| anyhow!("sequencer stopped"))
}
