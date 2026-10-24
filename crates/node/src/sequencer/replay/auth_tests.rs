//! Completion evidence must authenticate before it can consume either kind of pending approval.

use super::*;
use crate::log::{Checkpoint, CommandEntry};
use crate::sequencer::SignedWithdrawal;
use crate::tee::EnclaveKey;
use crate::test_support::{TRADER, deposit, withdraw};

#[test]
fn immediate_and_deferred_completion_authenticate_before_consuming_pending_approval() {
    for deferred in [false, true] {
        for mutation in 0..8 {
            let key = EnclaveKey::generate();
            let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
            let (mut replay, mut release) = pending(&key, domain, deferred, Some(AccountId(key.address())));
            match mutation {
                0 => release.signature.clear(),
                1 => release.chain_id += 1,
                2 => release.vault = AccountId([8; 20]),
                3 => {
                    release.signature = EnclaveKey::generate().sign_withdrawal(
                        release.chain_id,
                        &release.vault.0,
                        release.account,
                        release.amount,
                        release.nonce,
                    )
                }
                4 => {
                    release.account = AccountId([0xee; 20]);
                    release.signature = key.sign_withdrawal(
                        release.chain_id,
                        &release.vault.0,
                        release.account,
                        release.amount,
                        release.nonce,
                    );
                }
                5 => {
                    release.amount += Fixed::from_int(1);
                    release.signature = key.sign_withdrawal(
                        release.chain_id,
                        &release.vault.0,
                        release.account,
                        release.amount,
                        release.nonce,
                    );
                }
                6 => {
                    release.nonce += 1;
                    release.signature = key.sign_withdrawal(
                        release.chain_id,
                        &release.vault.0,
                        release.account,
                        release.amount,
                        release.nonce,
                    );
                }
                _ => release.approved_seq = Some(99),
            }
            assert!(replay.apply(&LogEntry::Withdrawal(release)).is_err(), "deferred {deferred}, mutation {mutation}");
            assert_eq!(replay.approved.len() + replay.deferred.len(), 1, "bogus completion preserves the approval");
            let (_, valid) = pending(&key, domain, deferred, Some(AccountId(key.address())));
            replay.apply(&LogEntry::Withdrawal(valid)).unwrap();
            assert!(replay.approved.is_empty() && replay.deferred.is_empty());
        }
    }
}

#[test]
fn a_valid_signature_without_independently_trusted_authority_cannot_clear_an_approval() {
    let key = EnclaveKey::generate();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    for deferred in [false, true] {
        let (mut replay, release) = pending(&key, domain, deferred, None);
        assert!(replay.apply(&LogEntry::Withdrawal(release)).unwrap_err().to_string().contains("trusted signer"));
        assert_eq!(replay.approved.len() + replay.deferred.len(), 1);
    }
}

fn pending(
    key: &EnclaveKey,
    domain: CheckpointDomain,
    deferred: bool,
    trusted: Option<AccountId>,
) -> (Replay, SignedWithdrawal) {
    let mut replay = Replay::with_trust(Some(domain), trusted);
    let mut engine = Engine::new();
    let mut nonce = None;
    for (seq, cmd) in [(0, deposit(100, "auth-test")), (1, withdraw(40))] {
        let sequenced = engine_core::command::Sequenced { seq, ts: 0, cmd };
        let events = engine.apply(&sequenced).unwrap();
        nonce = events
            .iter()
            .find_map(|event| match event {
                Event::WithdrawalApproved { nonce, .. } => Some(*nonce),
                _ => None,
            })
            .or(nonce);
        replay.apply(&LogEntry::Command(CommandEntry { sequenced, events: Some(events) })).unwrap();
    }
    if deferred {
        let root = engine.state_root();
        replay
            .apply(&LogEntry::Checkpoint(Checkpoint {
                parent: None,
                seq: 1,
                root: format!("0x{}", hex::encode(root)),
                signature: key.sign_checkpoint(domain.chain_id, &domain.vault.0, 1, &root),
                signer: key.address_hex(),
                domain: Some(domain),
                deferred_withdrawals: true,
            }))
            .unwrap();
    }
    let nonce = nonce.expect("withdrawal approved");
    (
        replay,
        SignedWithdrawal {
            account: TRADER,
            amount: Fixed::from_int(40),
            nonce,
            chain_id: domain.chain_id,
            vault: domain.vault,
            signature: key.sign_withdrawal(domain.chain_id, &domain.vault.0, TRADER, Fixed::from_int(40), nonce),
            approved_seq: deferred.then_some(1),
            authorization: None,
        },
    )
}
