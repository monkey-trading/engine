//! A screening retry that supersedes the receipt revision a custody transition was built from is a
//! race to reload, not inconsistent chain data.

use super::rescreens::{Fixture, Mode, pending_node};
use super::*;
use crate::custody::pending::retry_batch;
use crate::custody::stall::{PollError, Stall};
use crate::sequencer::Request;
use engine_core::deposits::{DepositReject, DepositStatus, DepositUpdate};
use tokio::sync::mpsc;

/// What happens to the receipt after the watcher read its revision and before its first
/// finalisation is sequenced.
#[derive(Clone, Copy)]
enum Interference {
    /// A screening retry settles the receipt, so the transition's revision is stale.
    Retry(Mode),
    /// The sequencer refuses the transition of a receipt nothing has changed.
    RefuseUnchanged,
}

/// Polls the fixture's watcher through a proxy that applies `interference` just before the first
/// finalisation, and returns that poll's result with a second poll's over the same proxy.
async fn poll_twice(
    name: &str,
    interference: Interference,
) -> (Fixture, String, Result<Polled, PollError>, Result<Polled, PollError>) {
    let mut fixture = pending_node(name, 1, true).await;
    for chain in &fixture.chains {
        chain.0.lock().unwrap().withdrawal_block = Some(2);
    }
    if let Interference::Retry(mode) = interference {
        fixture.mode.store(mode as u8, Ordering::SeqCst);
    }
    let reference =
        fixture.node.read(|engine| engine.unfinalised_deposit_records(0, u64::MAX)[0].receipt.reference()).await;
    let (proxy_tx, mut proxy_rx) = mpsc::channel::<Request>(64);
    let (real_tx, client) = (fixture.node.tx.clone(), fixture.client.clone());
    let proxy = tokio::spawn(async move {
        let mut intercepted = false;
        while let Some(request) = proxy_rx.recv().await {
            let request = match request {
                Request::Submit { cmd: Command::CustodyDeposit(update), reply }
                    if matches!(*update, DepositUpdate::Finalise { .. }) && !intercepted =>
                {
                    intercepted = true;
                    let update = match interference {
                        Interference::Retry(_) => {
                            retry_batch(Some(&client), &real_tx, &mut None).await.unwrap();
                            update
                        }
                        Interference::RefuseUnchanged => {
                            let receipt = update.subject().clone();
                            let rejected =
                                Event::CustodyDepositRejected { receipt, reason: DepositReject::SettledReceipt };
                            let _ = reply.expect("a reply is awaited").send(vec![rejected]);
                            continue;
                        }
                    };
                    Request::Submit { cmd: Command::CustodyDeposit(update), reply }
                }
                other => other,
            };
            real_tx.send(request).await.unwrap();
        }
    });
    let first = fixture.watch.poll(&proxy_tx).await;
    let second = fixture.watch.poll(&proxy_tx).await;
    drop(proxy_tx);
    proxy.await.unwrap();
    (fixture, reference, first, second)
}

async fn status(node: &Node, reference: String) -> DepositStatus {
    node.read(move |engine| engine.recorded_deposit(&reference).expect("recorded").status.clone()).await
}

#[tokio::test]
async fn a_retry_that_supersedes_a_receipt_revision_defers_the_transition_without_a_critical_alert() {
    for mode in [Mode::Clear, Mode::Sanctioned] {
        let (fixture, reference, first, second) =
            poll_twice(&format!("access-superseded-{}", mode as u8), Interference::Retry(mode)).await;
        assert!(matches!(first, Err(PollError::Stalled(Stall::Unsettled(_)))), "{first:?}");
        assert!(second.is_ok(), "the next poll reloads the revision and finalises: {second:?}");
        assert_eq!(status(&fixture.node, reference).await, DepositStatus::Finalised);
        let held = fixture.node.read(|engine| engine.held_deposits().len()).await;
        assert_eq!(held, usize::from(matches!(mode, Mode::Sanctioned)));
        fixture.node.stop().await;
        fixture.source.abort();
    }
}

#[tokio::test]
async fn a_refused_transition_of_an_unchanged_receipt_is_still_inconsistent_chain_data() {
    let (fixture, _, first, _) = poll_twice("access-refused-unchanged", Interference::RefuseUnchanged).await;
    assert!(matches!(first, Err(PollError::Stalled(Stall::Inconsistent(_)))), "{first:?}");
    fixture.node.stop().await;
    fixture.source.abort();
}
