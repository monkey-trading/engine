//! A node restarts from a paired cache holding custody receipts in every status. The reincluded
//! deposit, the finalised frontier and canonical coverage come back as the node left them, history
//! serves each receipt transition to its trader, and the node goes on settling receipts.

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate};
use engine_core::eip712::B256;
use engine_core::event::Event;
use tokio::sync::oneshot;

use crate::history::PageQuery;
use crate::node::paired_tests::harness::{Node, configuration};
use crate::sequencer::Request;
use crate::test_support::{TRADER, deposit, logged_history};

/// Statuses, the trader's cash and lock, the frontier and coverage, as the engine reports them.
type Custody = (Vec<Option<DepositStatus>>, Fixed, Fixed, Option<CustodyCheckpoint>, Option<u64>);

#[tokio::test]
async fn a_paired_restart_keeps_custody_receipts_frontier_and_coverage() {
    let cfg = configuration("paired-custody-restart");
    let node = Node::start(cfg).await.unwrap();
    let (kept, orphan, pending) = (receipt(1, 30), receipt(2, 20), receipt(3, 5));
    let moved =
        DepositReceipt { block: 20, block_hash: B256::repeat_byte(20), log_index: 9, deposit_id: 9, ..orphan.clone() };
    let receipts = [kept.clone(), orphan.clone(), moved.clone(), pending.clone()];
    custody(&node, Command::CustodyFinalised(frontier(0, 0))).await;
    for receipt in [&kept, &orphan, &pending] {
        custody(&node, update(DepositUpdate::Credit { receipt: receipt.clone() })).await;
    }
    let observed_at_seq = revision(&node, &orphan).await;
    let replace = DepositUpdate::Replace {
        receipt: orphan,
        observed_at_seq,
        replacement: moved.clone(),
        replacement_reversed_at_seq: None,
    };
    custody(&node, update(replace)).await;
    let observed_at_seq = revision(&node, &kept).await;
    custody(&node, update(DepositUpdate::Finalise { receipt: kept.clone(), observed_at_seq })).await;
    custody(&node, Command::CustodyFinalised(frontier(kept.block, 1))).await;
    let settled = query(&node, |engine| engine.last_seq().unwrap()).await;
    // Enough later commands that a checkpoint, and so a paired cache, covers every transition.
    for index in 0..node.cfg.node.checkpoint_every {
        node.submit(deposit(1, &format!("custody-restart-{index}"))).await;
    }
    let before = state(&node, &receipts).await;
    assert_eq!(before.4, Some(3), "the frontier count and the two live receipts above it");
    node.history_caught_up().await;
    let (cfg, reader) = node.stop().await;
    drop(reader);

    let node = Node::start(cfg).await.unwrap();
    let cache = node.snapshot_seq.expect("restart uses a paired cache");
    assert!(cache >= settled, "the cache at {cache} holds every receipt transition through {settled}");
    assert_eq!(state(&node, &receipts).await, before, "receipts, frontier and coverage survive the cache");
    let expected: Vec<_> = logged_history(&node.cfg.node.log_dir, TRADER)
        .into_iter()
        .filter(|event| matches!(event.event, Event::CustodyDepositChanged { .. }))
        .collect();
    assert_eq!(expected.len(), 6, "three credits, the two-sided move and one finalisation");
    let query = PageQuery {
        account: TRADER,
        kind: Some("custody_deposit_changed".into()),
        before: None,
        after: None,
        limit: 200,
    };
    let page = node.reader.page(query).await.unwrap().expect("history serves at once after a paired restart");
    assert_eq!(page.events, expected, "each receipt transition is the trader's, under its own kind");

    for receipt in [&moved, &pending] {
        let observed_at_seq = revision(&node, receipt).await;
        custody(&node, update(DepositUpdate::Finalise { receipt: receipt.clone(), observed_at_seq })).await;
    }
    custody(&node, Command::CustodyFinalised(frontier(moved.block, 3))).await;
    let (_, balance, locked, _, coverage) = state(&node, &receipts).await;
    assert_eq!((balance, locked, coverage), (before.1, Fixed::ZERO, Some(3)));
    node.stop_after_signing("after-custody-restart").await;
}

/// Sequences a custody command the engine must accept.
async fn custody(node: &Node, cmd: Command) {
    let events = node.submit(cmd).await;
    assert!(
        !events.iter().any(|event| matches!(event, Event::AdminRejected { .. } | Event::CustodyDepositRejected { .. })),
        "{events:?}"
    );
}

async fn query<T: Send + 'static>(node: &Node, read: impl FnOnce(&engine_core::Engine) -> T + Send + 'static) -> T {
    let (reply, answer) = oneshot::channel();
    let ask: crate::sequencer::Query = Box::new(move |engine| drop(reply.send(read(engine))));
    node.tx.send(Request::Query(ask)).await.unwrap();
    answer.await.unwrap()
}

/// The sequenced revision a conditional transition of `receipt` names.
async fn revision(node: &Node, receipt: &DepositReceipt) -> u64 {
    let reference = receipt.reference();
    query(node, move |engine| engine.recorded_deposit(&reference).unwrap().updated_at_seq).await
}

async fn state(node: &Node, receipts: &[DepositReceipt]) -> Custody {
    let references: Vec<_> = receipts.iter().map(DepositReceipt::reference).collect();
    query(node, move |engine| {
        let statuses = references
            .iter()
            .map(|reference| engine.recorded_deposit(reference).map(|recorded| recorded.status.clone()))
            .collect();
        let account = &engine.accounts()[&TRADER];
        (
            statuses,
            account.balance,
            account.unfinalised_deposits,
            engine.custody_checkpoint(),
            engine.custody_deposit_coverage(),
        )
    })
    .await
}

fn update(update: DepositUpdate) -> Command {
    Command::CustodyDeposit(Box::new(update))
}

fn frontier(block: u64, deposit_count: u64) -> CustodyCheckpoint {
    CustodyCheckpoint { block, block_hash: B256::repeat_byte(0x40 + block as u8), deposit_count }
}

fn receipt(id: u64, amount: i64) -> DepositReceipt {
    DepositReceipt {
        block: 10 + id,
        block_hash: B256::repeat_byte(id as u8),
        tx_hash: B256::repeat_byte(0x10 + id as u8),
        log_index: id,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: DepositBeneficiary::Trader { account: TRADER },
        amount: Fixed::from_int(amount),
    }
}
