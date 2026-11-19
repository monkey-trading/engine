//! The transitions of the receipts a canonical scan disproves: before `V83` one command each, from it
//! one reorganisation command for every receipt canonical again, after each the chain dropped reverses.

use anyhow::anyhow;
use engine_core::access::{AccessEvidence, receipt_access_deposit};
use engine_core::command::Command;
use engine_core::deposits::{DepositReceipt, DepositReject, DepositUpdate, ReceiptMove, RecordedDeposit};
use engine_core::eip712::deposit_screening_hash;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use tokio::sync::mpsc;
use tracing::{Instrument, info, info_span};

use crate::access::{ScreeningRequest, screen};
use crate::custody::credit::{query, submit};
use crate::custody::evm::lifecycle::revision::{observed_revision, refusal_stall, refused, replacement_update};
use crate::custody::evm::lifecycle::{Orphaned, sequence_update};
use crate::custody::evm::{DepositWatcher, VaultReader};
use crate::custody::stall::PollError;
use crate::sequencer::Request;

impl<V: VaultReader> DepositWatcher<V> {
    /// Before `V83`: each orphan's transition as its own command, in reference order, a move screened
    /// against its replacement's payer. A deferred orphan waits.
    /// Error cases:
    /// - the sequencer stops, or refuses a transition (see `refusal_stall`)
    pub(super) async fn transition_each(
        &self,
        tx: &mpsc::Sender<Request>,
        orphans: Vec<(&RecordedDeposit, Orphaned)>,
    ) -> Result<(), PollError> {
        for (recorded, orphaned) in orphans {
            let update = match orphaned {
                Orphaned::Replaced(replacement) if self.access_enforced => {
                    let update = replacement_update(tx, recorded, replacement.clone()).await?;
                    let (subject, observed) = (update.subject().clone(), observed_revision(&update));
                    let (payer, evidence) = self.screen_replacement(&replacement).await;
                    let command = Command::AccessCustodyReplacement { update: Box::new(update), payer, evidence };
                    let events = submit(tx, command).await?;
                    match refused(&events) {
                        Some(DepositReject::ProtocolNotActive) => {
                            return Err(PollError::unsettled(anyhow!(
                                "canonical replacement awaits access protocol activation"
                            )));
                        }
                        Some(reason) => {
                            let what = "sequencer refused screened canonical replacement";
                            return Err(refusal_stall(tx, &subject, observed, &reason, what).await);
                        }
                        None => {}
                    }
                    continue;
                }
                Orphaned::Replaced(replacement) => replacement_update(tx, recorded, replacement).await?,
                Orphaned::Reversed => DepositUpdate::Reverse {
                    receipt: recorded.receipt.clone(),
                    observed_at_seq: recorded.updated_at_seq,
                },
                Orphaned::Deferred => continue,
            };
            sequence_update(tx, update).await?;
        }
        Ok(())
    }

    /// From `V83`: reverses each orphan the chain dropped, then moves every orphan canonical again in one
    /// `CustodyReorganisation`, each replacement screened against its payer when the node screens.
    /// Error cases:
    /// - an orphan still waits for the eligible head (the caller holds the scan instead)
    /// - the sequencer stops, or refuses a transition (see `refusal_stall`)
    pub(super) async fn reorganise(
        &self,
        tx: &mpsc::Sender<Request>,
        orphans: Vec<(&RecordedDeposit, Orphaned)>,
    ) -> Result<(), PollError> {
        if orphans.iter().any(|(_, orphaned)| *orphaned == Orphaned::Deferred) {
            return Err(PollError::unsettled(anyhow!("a reorganisation's orphan waits for the eligible head")));
        }
        let mut moving = Vec::new();
        for (recorded, orphaned) in orphans {
            match orphaned {
                Orphaned::Reversed => {
                    let receipt = recorded.receipt.clone();
                    sequence_update(tx, DepositUpdate::Reverse { receipt, observed_at_seq: recorded.updated_at_seq })
                        .await?;
                }
                Orphaned::Replaced(replacement) => moving.push((recorded, replacement)),
                Orphaned::Deferred => unreachable!("checked above"),
            }
        }
        if moving.is_empty() {
            return Ok(());
        }
        let mut moves = Vec::new();
        for (recorded, replacement) in moving {
            let DepositUpdate::Replace { receipt, observed_at_seq, replacement, replacement_reversed_at_seq } =
                replacement_update(tx, recorded, replacement).await?
            else {
                unreachable!("a replacement update is a `Replace`");
            };
            let (payer, evidence) = if self.access_enforced {
                let (payer, evidence) = self.screen_replacement(&replacement).await;
                (payer, Some(evidence))
            } else {
                (None, None)
            };
            moves.push(ReceiptMove {
                receipt,
                observed_at_seq,
                replacement,
                replacement_reversed_at_seq,
                payer,
                evidence,
            });
        }
        let span = info_span!("reorganisation", id = %moves[0].receipt.reference(), moves = moves.len());
        async move {
            let events = submit(tx, Command::CustodyReorganisation { moves: moves.clone() }).await?;
            let Some((subject, reason)) = events.iter().find_map(|event| match event {
                Event::CustodyDepositRejected { receipt, reason } => Some((receipt.clone(), reason.clone())),
                _ => None,
            }) else {
                info!("reorganisation moved its receipts together");
                return Ok(());
            };
            if reason == DepositReject::ProtocolNotActive {
                return Err(PollError::unsettled(anyhow!("a reorganisation awaits protocol activation")));
            }
            let observed = moves.iter().find(|move_| move_.receipt == subject).map(|move_| move_.observed_at_seq);
            Err(refusal_stall(tx, &subject, observed, &reason, "sequencer refused a reorganisation").await)
        }
        .instrument(span)
        .await
    }

    /// The vault caller the scan read for `replacement`, and its screening by the access source.
    async fn screen_replacement(&self, replacement: &DepositReceipt) -> (Option<AccountId>, AccessEvidence) {
        let scan = self.confirmed.scan.as_ref().expect("scan retained");
        let payer = scan.payers.get(&replacement.reference()).copied().flatten();
        let deposit = receipt_access_deposit(replacement, payer);
        let request = ScreeningRequest {
            request_hash: deposit_screening_hash(&deposit),
            account: deposit.account,
            payer,
            peer: None,
        };
        (payer, screen(self.access.as_deref(), &request).await)
    }
}

/// The result of a poll's transitions and credits under an earlier version's flow: a stall once `V83`
/// is in force, which an activation landing after the poll read the version caused, waits for the next
/// poll, which moves a reorganisation together, rather than paging as inconsistent chain data.
/// Error cases:
/// - the sequencer stops while the version is read again
/// - `applied`'s own error, when `V83` is still not in force
pub(super) async fn raced_activation(
    tx: &mpsc::Sender<Request>,
    applied: Result<(), PollError>,
) -> Result<(), PollError> {
    let Err(PollError::Stalled(_)) = applied else { return applied };
    let activated =
        query(tx, |engine| engine.protocol_version() >= ProtocolVersion::V83).await.map_err(PollError::Fatal)?;
    if activated {
        return Err(PollError::unsettled(anyhow!("a reorganisation waits to move as one command")));
    }
    applied
}
