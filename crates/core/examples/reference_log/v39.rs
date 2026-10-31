//! The session's `V39` part: the exit terms name no unheld principal past the deposit cursor.

use alloy_primitives::B256;
use engine_core::command::AdminCommand;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::common::Harness;

/// `V39` activates and the exit terms leaf keeps committing against the accumulated state, with
/// every account's exit proof verifying. The corrected empty-index exclusion this version gates —
/// a reversed deposit at vault deposit id 0 with no live receipt is the vault's cursor rule's
/// refund, not unheld principal too — needs an empty live index no accumulated root holds, so its
/// regression lives in `tests/exit/custody.rs`
/// (`a_sole_reversed_deposit_at_id_zero_reserves_nothing_past_the_cursor`) and every earlier
/// reference log replays unchanged.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 });
    let root = B256::from(h.engine.state_root());
    let terms = h.engine.exit_terms_proof().expect("V39 commits the exit terms");
    assert!(terms.verify(root));
    let accounts: Vec<AccountId> = h.engine.accounts().keys().copied().collect();
    for account in accounts {
        let proof = h.engine.exit_proof(account).expect("V39 proves every account's exit");
        assert!(proof.account.as_ref().is_none_or(|held| held.verify(root) && held.equity().is_some()));
        assert!(proof.approvals.iter().all(|approval| approval.verify(root)));
        assert!(proof.queued.iter().all(|queued| queued.verify(root)));
        assert!(proof.pending_deposits.iter().all(|pending| pending.verify(root)));
    }
}
