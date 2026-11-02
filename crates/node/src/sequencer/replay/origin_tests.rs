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

#[test]
fn legacy_staking_references_need_genuine_seals_before_writable_recovery() {
    use crate::log::Manifest;
    use crate::sequencer::Sequencer;
    use crate::sequencer::fixture_tests::old_genesis_log;
    use crate::test_support::{dev_config, log_entries, scratch_dir, sequencer_config, trusted_now};
    use engine_core::auth::ClientAuth;
    use engine_core::emissions::TokenUnits;
    use engine_core::staking::StakingConfig;
    use std::sync::Arc;
    use tokio::sync::{broadcast, mpsc, watch};

    let original_dir = scratch_dir("legacy-staking-provenance-original");
    let mut genesis_config = dev_config().genesis;
    genesis_config.staking = Some(StakingConfig {
        liquidity_floor: Fixed::ONE,
        epoch_ms: 60_000,
        start_ms: trusted_now().latest_ms + 60_000,
        cooldown_ms: 86_400_000,
    });
    let genesis = crate::sequencer::genesis_commands(&genesis_config);
    let account = AccountId([7; 20]);
    old_genesis_log(
        &original_dir,
        &ClientAuth::Unsigned,
        ProtocolVersion::V25,
        genesis.clone(),
        vec![Command::Stake { account, amount: TokenUnits(500), block: 10, chain_ref: "original-stake".into() }],
    );
    let original = log_entries(&original_dir);
    let key = Arc::new(EnclaveKey::test_key());
    let signer = AccountId(key.address());
    let domain = original
        .iter()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => checkpoint.domain,
            _ => None,
        })
        .unwrap();
    let config = crate::sequencer::SequencerConfig {
        genesis: genesis.clone(),
        ..sequencer_config(&original_dir, watch::channel(Manifest::default()).0)
    };
    let recovered =
        Sequencer::recover(config, key.clone(), broadcast::channel(16).0, mpsc::unbounded_channel().0).unwrap();
    assert_eq!(recovered.engine.protocol_version(), ProtocolVersion::V25);
    drop(recovered);

    // The old key seals a legacy stake after its authorized checkpoint, then the new key covers
    // the later root. Earlier immutable authority authenticates that genuine old-key tail.
    let current = Arc::new(EnclaveKey::generate());
    let current_signer = AccountId(current.address());
    let mut rotated = original.clone();
    let final_checkpoint = rotated
        .iter_mut()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .unwrap();
    final_checkpoint.signature = current.sign_checkpoint(
        domain.chain_id,
        &domain.vault.0,
        final_checkpoint.seq,
        &final_checkpoint.root.parse::<B256>().unwrap().0,
    );
    final_checkpoint.signer = current.address_hex();
    let parent = CheckpointParent {
        has_checkpoint: true,
        seq: final_checkpoint.seq,
        root: final_checkpoint.root.parse().unwrap(),
    };
    let history = crate::checkpoints::SignerHistory {
        epochs: [(1, signer), (2, current_signer)].into(),
        checkpoints: rotated
            .iter()
            .filter_map(|entry| match entry {
                LogEntry::Checkpoint(checkpoint) => {
                    let named = checkpoint.signer.parse::<AccountId>().unwrap();
                    Some(crate::checkpoints::AcceptedCheckpoint {
                        seq: checkpoint.seq,
                        root: checkpoint.root.clone(),
                        signer: named,
                        signer_epoch: if named == signer { 1 } else { 2 },
                    })
                }
                _ => None,
            })
            .collect(),
    };
    let rotated_dir = scratch_dir("legacy-staking-rotated-tail");
    std::fs::create_dir_all(&rotated_dir).unwrap();
    std::fs::write(
        rotated_dir.join("00000000000000000000.jsonl"),
        rotated.iter().map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap())).collect::<String>(),
    )
    .unwrap();
    let log = Sequencer::read_log_with_authorities(
        &rotated_dir,
        Some(domain),
        Some(current_signer),
        Some(parent),
        history,
        |_| {},
        None,
        None,
    )
    .unwrap();
    let config = crate::sequencer::SequencerConfig {
        genesis: genesis.clone(),
        ..sequencer_config(&rotated_dir, watch::channel(Manifest::default()).0)
    };
    let recovered =
        Sequencer::recover_from(log, config, current, broadcast::channel(16).0, mpsc::unbounded_channel().0).unwrap();
    assert!(recovered.engine.is_staking_credited("original-stake"));
    assert_eq!(recovered.engine.protocol_version(), ProtocolVersion::V25);
    drop(recovered);

    for forged_seal in [false, true] {
        let mut entries = original.clone();
        let mut link = B256::ZERO;
        let attacker = EnclaveKey::generate();
        for entry in &mut entries {
            match entry {
                LogEntry::Command(command) => {
                    if let Command::Stake { chain_ref, .. } = &mut command.sequenced.cmd {
                        *chain_ref = "host-replaced-reference".into();
                        command.seal = forged_seal.then(|| {
                            attacker
                                .seal_command(domain, crate::log::command_seal::command_link(link, command).unwrap())
                        });
                    }
                    link = crate::log::command_seal::command_link(link, command).unwrap();
                }
                LogEntry::Checkpoint(checkpoint) => {
                    link = crate::log::command_seal::checkpoint_link(checkpoint).unwrap();
                }
                LogEntry::Withdrawal(_) => {}
            }
        }
        // V25 roots authenticate balances but omit these references, so read-only root audit passes.
        let audited = super::replay_with_signer(&entries, domain, signer).unwrap();
        assert!(audited.engine.is_staking_credited("host-replaced-reference"));
        let dir = scratch_dir(if forged_seal { "legacy-staking-forged-seal" } else { "legacy-staking-no-seal" });
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("00000000000000000000.jsonl");
        let text =
            entries.iter().map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap())).collect::<String>();
        std::fs::write(&path, &text).unwrap();
        let config = crate::sequencer::SequencerConfig {
            genesis: genesis.clone(),
            ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
        };
        let error = Sequencer::recover(config, key.clone(), broadcast::channel(16).0, mpsc::unbounded_channel().0)
            .err()
            .expect("writable recovery must reject altered legacy staking provenance");
        assert!(error.to_string().contains("lacks authenticated reference provenance"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            text,
            "rejection precedes writable recovery and V27 activation"
        );
    }
}
