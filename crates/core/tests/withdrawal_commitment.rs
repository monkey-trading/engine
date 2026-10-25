//! Approval inclusion, byte-exact leaves, replay and incremental roots through `Engine::apply`.

mod common;
#[path = "withdrawals/setup.rs"]
mod setup;

use alloy_primitives::B256;
use common::{CHAIN_ID, Client, Harness, VAULT, acct, f};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Reject};
use engine_core::merkle::{withdrawal_approval_hash, withdrawal_approval_key};
use engine_core::protocol::ProtocolVersion;
use engine_core::withdrawal::WithdrawalApproval;
use engine_core::{Engine, Fixed};
use setup::{approvals, capital, deposit, v22, withdraw};
use sha3::{Digest, Keccak256};

#[test]
fn approval_leaf_matches_the_packed_key_and_four_word_solidity_value() {
    let a = acct(0x17);
    let approval = WithdrawalApproval {
        account: a,
        amount: f("123456.123456"),
        nonce: 0x0102030405060708,
        approved_seq: 0x1122334455667788,
    };
    let mut key = vec![32];
    key.extend(a.0);
    key.extend(approval.nonce.to_be_bytes());
    assert_eq!(withdrawal_approval_key(a, approval.nonce), B256::from(<[u8; 32]>::from(Keccak256::digest(&key))));

    // Four 32-byte Solidity ABI words: address, uint256 amountUSDC, uint64 nonce, uint64 seq.
    let mut value = [0u8; 128];
    value[12..32].copy_from_slice(&a.0);
    value[48..64].copy_from_slice(&123_456_123_456u128.to_be_bytes());
    value[88..96].copy_from_slice(&approval.nonce.to_be_bytes());
    value[120..128].copy_from_slice(&approval.approved_seq.to_be_bytes());
    assert_eq!(withdrawal_approval_hash(&approval), B256::from(<[u8; 32]>::from(Keccak256::digest(value))));
}

#[test]
fn inclusion_binds_every_payable_field_and_requires_the_last_computed_root() {
    let mut h = v22();
    let a = acct(1);
    deposit(&mut h, a, "10");
    let before = B256::from(h.engine.state_root());
    withdraw(&mut h, a, "5");
    assert!(h.engine.approval_proof(a, 1).is_none());
    let root = B256::from(h.engine.state_root());
    assert_ne!(root, before);
    let proof = h.engine.approval_proof(a, 1).unwrap();
    assert_eq!((proof.root, proof.seq, proof.approval.approved_seq), (root, h.engine.last_seq().unwrap(), proof.seq));
    assert!(proof.verify(root));
    assert!(!proof.verify(before));
    assert!(h.engine.approval_proof(acct(2), 1).is_none());
    assert!(h.engine.approval_proof(a, 2).is_none());

    let mut changed = proof.clone();
    changed.approval.account = acct(2);
    assert!(!changed.verify(root));
    let mut changed = proof.clone();
    changed.approval.amount += f("0.000001");
    assert!(!changed.verify(root));
    let mut changed = proof.clone();
    changed.approval.nonce += 1;
    assert!(!changed.verify(root));
    let mut changed = proof.clone();
    changed.approval.approved_seq += 1;
    assert!(!changed.verify(root));
    let mut changed = proof.clone();
    changed.siblings[0].0[0] ^= 1;
    assert!(!changed.verify(root));
    for malformed in [f("0.000000001"), f("-1"), Fixed::ZERO] {
        let mut changed = proof.clone();
        changed.approval.amount = malformed;
        assert!(!changed.verify(root));
    }

    // The caller supplies its trusted root; the response's root field is informational.
    let mut changed = proof.clone();
    changed.root = before;
    assert!(changed.verify(root));
    withdraw(&mut h, a, "1");
    let next = B256::from(h.engine.state_root());
    assert_eq!(h.engine.withdrawal_approval(a, 1), Some(&proof.approval));
    assert!(proof.verify(root));
    assert!(!proof.verify(next));
    assert!(h.engine.approval_proof(a, 1).unwrap().verify(next));
}

#[test]
fn replay_and_serialized_restart_preserve_partial_queue_approvals_and_future_events() {
    let mut h = setup::two_waiting();
    capital(&mut h, "0.000000999");
    let root = h.engine.state_root();
    let mut restored: Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    assert_eq!(restored.state_root(), root);
    let mut replay = Engine::new();
    for command in &h.log {
        replay.apply(command).unwrap();
        replay.state_root();
    }
    assert_eq!(replay.state_root(), root);
    assert_eq!(
        restored.withdrawal_queue().copied().collect::<Vec<_>>(),
        h.engine.withdrawal_queue().copied().collect::<Vec<_>>()
    );
    let a = acct(1);
    assert!(restored.approval_proof(a, 1).unwrap().verify(B256::from(root)));
    assert_eq!(restored.withdrawal_cash(), Some(f("0.000000999")));

    let expected = deposit(&mut h, acct(3), "40");
    assert_eq!(approvals(&expected), [(a, f("40"), 2)]);
    let command = h.log.last().unwrap();
    assert_eq!(restored.apply(command).unwrap(), expected);
    assert_eq!(replay.apply(command).unwrap(), expected);
    let next = h.engine.state_root();
    assert_eq!(restored.state_root(), next);
    assert_eq!(replay.state_root(), next);
    assert_ne!(root, next);
    assert_eq!(h.engine.withdrawal_cash(), Some(f("0.000000999")));
    assert_eq!(h.engine.withdrawal_queue().next().unwrap().remaining, f("40"));
    assert_eq!(h.engine.withdrawal_approvals().count(), 2);
    for nonce in [1, 2] {
        let proof = replay.approval_proof(a, nonce).unwrap();
        assert!(proof.verify(B256::from(next)));
        assert_eq!(proof.approval.amount, if nonce == 1 { f("30") } else { f("40") });
    }
    assert!(replay.approval_proof(acct(2), 1).is_none());
}

#[test]
fn unchanged_approval_history_and_accounts_are_not_reencoded_at_each_root() {
    let mut h = v22();
    deposit(&mut h, acct(1), "100");
    for _ in 0..40 {
        withdraw(&mut h, acct(1), "1");
        h.engine.state_root();
    }
    assert_eq!(h.engine.withdrawal_approvals().count(), 40);
    h.cmd(Command::Tick);
    h.engine.state_root();
    assert_eq!(h.engine.last_root_work().unwrap().leaves, 1);

    withdraw(&mut h, acct(1), "1");
    let hot = h.engine.state_root();
    assert_eq!(h.engine.last_root_work().unwrap().leaves, 4);
    let mut cold: Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    assert_eq!(cold.state_root(), hot);
    assert!(cold.last_root_work().unwrap().leaves > 40);
}

#[test]
fn queue_state_and_physical_budget_each_change_the_commitment() {
    let mut h = setup::two_waiting();
    let root = h.engine.state_root();
    let state = serde_json::to_value(&h.engine).unwrap();
    let seq = h.engine.withdrawal_queue().next().unwrap().requested_seq.to_string();
    for field in ["amount", "remaining"] {
        let mut changed = state.clone();
        changed["withdrawal_queue"][&seq][field] = serde_json::json!("1");
        let mut cold: Engine = serde_json::from_value(changed).unwrap();
        assert_ne!(cold.state_root(), root, "queue {field} must be committed");
    }
    let mut changed = state.clone();
    changed["withdrawal_queue"][&seq]["ticket"] = serde_json::json!(7);
    let mut cold: Engine = serde_json::from_value(changed).unwrap();
    assert_ne!(cold.state_root(), root, "queue tickets must be committed");
    let mut changed = state.clone();
    changed["withdrawal_next_ticket"] = serde_json::json!(9);
    let mut cold: Engine = serde_json::from_value(changed).unwrap();
    assert_ne!(cold.state_root(), root, "the next enqueue ticket must be committed");
    let mut changed = state;
    changed["withdrawal_cash"] = serde_json::json!("0.000000001");
    let mut cold: Engine = serde_json::from_value(changed).unwrap();
    assert_ne!(cold.state_root(), root);
}

#[test]
fn original_signed_amount_is_authenticated_before_rounding_and_replays_cannot_reserve_twice() {
    let mut h = Harness::v8(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT });
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V22 });
    let client = Client::new(1);
    deposit(&mut h, client.account, "1.000000999");
    let signed = client.withdraw(0, "1.000000999");
    let mut changed = signed.clone();
    if let Command::Withdraw { amount, .. } = &mut changed {
        *amount = f("1.000000998");
    }
    assert_eq!(
        h.cmd(changed),
        [Event::WithdrawalRejected {
            account: client.account,
            amount: f("1.000000998"),
            reason: Reject::InvalidSignature,
        }]
    );
    assert_eq!(h.engine.next_request_nonce(client.account), 0);
    assert_eq!(approvals(&h.cmd(signed.clone())), [(client.account, f("1"), 1)]);
    assert_eq!(h.balance(client.account), f("0.000000999"));
    assert_eq!(
        h.cmd(signed),
        [Event::WithdrawalRejected {
            account: client.account,
            amount: f("1.000000999"),
            reason: Reject::InvalidNonce { expected: 1 },
        }]
    );
    assert_eq!(h.engine.withdrawal_approvals().count(), 1);
    assert_eq!(h.engine.accounts()[&client.account].withdrawal_nonce, 1);
}
