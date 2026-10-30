//! The session's `V38` part: the state root commits what the vault's exit against the last posted
//! root checks.

use engine_core::access::SanctionsStatus;
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt};
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::merkle::ExitTerms;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::common::{Harness, f};
use crate::v26::screening_evidence;

/// `V38` activates and rebuilds the tree: the exit terms leaf holds the deposit cursor and the
/// lowest gaps below it, and totals the pending and held principal, and every account's exit proofs
/// verify against the root. A house receipt whose payer the source cannot classify then waits
/// pending: the pending total holds its principal, and it proves as the vault refunds it.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V38 });
    let root = B256::from(h.engine.state_root());
    let before = h.engine.exit_terms_proof().expect("V38 commits the exit terms");
    assert!(before.verify(root));
    let accounts: Vec<AccountId> = h.engine.accounts().keys().copied().collect();
    for account in accounts {
        let proof = h.engine.exit_proof(account).expect("V38 proves every account's exit");
        assert!(proof.account.as_ref().is_none_or(|held| held.verify(root) && held.equity().is_some()));
        assert!(proof.approvals.iter().all(|approval| approval.verify(root)));
        assert!(proof.queued.iter().all(|queued| queued.verify(root)));
        assert!(proof.pending_deposits.iter().all(|pending| pending.verify(root)));
    }

    let receipt = DepositReceipt {
        block: 2_000_000,
        block_hash: B256::repeat_byte(0x36),
        tx_hash: B256::repeat_byte(0x37),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 2_000_000,
        beneficiary: DepositBeneficiary::House,
        amount: f("25"),
    };
    let payer = AccountId([0x36; 20]);
    let evidence = screening_evidence(h, &receipt, payer, SanctionsStatus::Unknown);
    let events = h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: Some(payer), evidence });
    assert!(events.iter().any(|event| matches!(event, Event::DepositPending { .. })), "{events:?}");
    let root = B256::from(h.engine.state_root());
    let after = h.engine.exit_terms_proof().expect("V38 commits the exit terms").terms;
    // Its id is below the cursor and above the lowest gaps, so only the pending total moves.
    assert!(receipt.deposit_id < before.terms.deposit_cursor);
    assert_eq!(
        after,
        ExitTerms {
            pending: before.terms.pending + f("25"),
            held_receipts: before.terms.held_receipts + f("25"),
            ..before.terms.clone()
        }
    );
    let refunds = h.engine.house_pending_deposit_proofs().expect("V38 proves pending deposits");
    let refund = refunds.iter().find(|proof| proof.chain_ref == receipt.reference()).expect("the house receipt");
    assert!(refund.verify(root) && refund.deposit.payer == Some(payer));
}
