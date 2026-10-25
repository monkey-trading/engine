//! Covering checkpoints authenticate ordinary inputs only after existing authority fences.

use super::*;

use engine_core::command::{Command, Sequenced};
use engine_core::eip712::B256;

use crate::log::{Checkpoint, CheckpointParent, CommandEntry};
use crate::tee::EnclaveKey;

#[test]
fn only_an_independently_authorized_checkpoint_covers_a_legacy_tick() {
    for trusted_checkpoint in [false, true] {
        let key = EnclaveKey::test_key();
        let other = EnclaveKey::generate();
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let mut replay = Replay::with_trust(Some(domain), Some(AccountId(key.address())));
        let command = Sequenced { seq: 0, ts: 0, cmd: Command::Tick };
        let mut oracle = Engine::new();
        let events = oracle.apply(&command).unwrap();
        replay
            .apply(&LogEntry::Command(CommandEntry { sequenced: command, events: Some(events), seal: None }))
            .unwrap();
        assert!(replay.origin.require_recovery(domain, AccountId(key.address())).is_err());
        let signer = if trusted_checkpoint { &key } else { &other };
        let root = oracle.state_root();
        let checkpoint = Checkpoint {
            parent: None,
            seq: 0,
            root: format!("0x{}", hex::encode(root)),
            signature: signer.sign_checkpoint(domain.chain_id, &domain.vault.0, 0, &root),
            signer: signer.address_hex(),
            domain: Some(domain),
            deferred_withdrawals: false,
        };
        let result = replay.apply(&LogEntry::Checkpoint(checkpoint));
        assert_eq!(result.is_ok(), trusted_checkpoint);
        assert_eq!(replay.origin.require_recovery(domain, AccountId(key.address())).is_ok(), trusted_checkpoint);
    }
}

#[test]
fn a_parent_refusal_cannot_clear_uncovered_command_origin() {
    let key = EnclaveKey::test_key();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    let parent = CheckpointParent::default();
    let mut replay = Replay::with_recovery(Some(domain), Some(AccountId(key.address())), parent);
    let command = Sequenced { seq: 0, ts: 0, cmd: Command::Tick };
    let mut oracle = Engine::new();
    let events = oracle.apply(&command).unwrap();
    replay.apply(&LogEntry::Command(CommandEntry { sequenced: command, events: Some(events), seal: None })).unwrap();
    let root = oracle.state_root();
    let divergent = CheckpointParent { has_checkpoint: true, seq: 9, root: B256::from([1; 32]) };
    let checkpoint = Checkpoint {
        parent: Some(divergent),
        seq: 0,
        root: format!("0x{}", hex::encode(root)),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, 0, &root, divergent),
        signer: key.address_hex(),
        domain: Some(domain),
        deferred_withdrawals: true,
    };
    assert!(replay.apply(&LogEntry::Checkpoint(checkpoint)).is_err());
    assert!(replay.origin.require_recovery(domain, AccountId(key.address())).is_err());
}
