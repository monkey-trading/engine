//! Authenticated staking identities and completed finalized scans from V27.
mod common;

use alloy_primitives::B256;
use common::{Harness, acct, f};
use engine_core::Engine;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;

/// A staking harness with either legacy or authenticated mirror identity semantics.
fn harness(version: ProtocolVersion) -> Harness {
    let mut h = Harness::staking(f("1000000"), 600_000, 86_400_000);
    if version > ProtocolVersion::V25 {
        h.admin(AdminCommand::ActivateProtocol { version });
    }
    h
}

/// Credits one mirror and commits it.
fn stake(h: &mut Harness, reference: &str) -> [u8; 32] {
    let events =
        h.cmd(Command::Stake { account: acct(80), amount: TokenUnits(10), block: 8, chain_ref: reference.into() });
    assert!(matches!(events.as_slice(), [Event::Staked { .. }]), "{events:?}");
    h.engine.state_root()
}

#[test]
fn mirror_identity_changes_the_root_only_under_the_authenticated_protocol() {
    for version in [ProtocolVersion::V25, ProtocolVersion::V27] {
        let mut left = harness(version);
        let mut right = harness(version);
        let roots = (stake(&mut left, "0xmirror:a"), stake(&mut right, "0xmirror:b"));
        assert_eq!(roots.0 == roots.1, version == ProtocolVersion::V25);
        let rebuilt: Engine = serde_json::from_value(serde_json::to_value(&left.engine).unwrap()).unwrap();
        let mut rebuilt = rebuilt;
        assert_eq!(rebuilt.state_root(), roots.0, "incremental and rebuilt identity leaves agree");
    }
}

#[test]
fn activation_authenticates_mirrors_credited_before_activation() {
    let mut left = harness(ProtocolVersion::V25);
    let mut right = harness(ProtocolVersion::V25);
    assert_eq!(stake(&mut left, "0xold:a"), stake(&mut right, "0xold:b"));
    for h in [&mut left, &mut right] {
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V27 });
    }
    assert_ne!(left.engine.state_root(), right.engine.state_root());
    let mut rebuilt: Engine = serde_json::from_value(serde_json::to_value(&left.engine).unwrap()).unwrap();
    assert_eq!(left.engine.state_root(), rebuilt.state_root(), "activation rebuild includes every old identity");
}

#[test]
fn changing_a_recorded_mirror_identity_cannot_preserve_an_authenticated_checkpoint() {
    let mut h = harness(ProtocolVersion::V27);
    let root = stake(&mut h, "0xretained");
    let mut altered = serde_json::to_value(&h.engine).unwrap();
    altered["staking_refs"] = serde_json::json!(["0xforged"]);
    let mut recovered: Engine = serde_json::from_value(altered).unwrap();
    assert_ne!(recovered.state_root(), root, "host-altered deduplication cannot match the signed root");
    let mut replayed = Engine::new();
    for mut sequenced in h.log {
        if let Command::Stake { chain_ref, .. } = &mut sequenced.cmd {
            *chain_ref = "0xforged".into();
        }
        replayed.apply(&sequenced).unwrap();
    }
    assert_ne!(replayed.state_root(), root, "altered full-log recovery cannot match the signed checkpoint");
}

#[test]
fn only_monotonic_matching_completed_scan_anchors_enter_the_state_root() {
    let mut h = harness(ProtocolVersion::V27);
    stake(&mut h, "0xprogress");
    let initial = h.engine.state_root();
    let hash = B256::repeat_byte(8);
    assert!(h.cmd(Command::StakingProgress { block: 8, block_hash: hash }).is_empty());
    assert_ne!(h.engine.state_root(), initial);
    let anchor = h.engine.staking().unwrap().completed_through.unwrap();
    assert_eq!((anchor.block, anchor.block_hash), (8, hash));
    assert!(h.cmd(Command::StakingProgress { block: 8, block_hash: hash }).is_empty());
    for (block, block_hash) in [(7, hash), (8, B256::repeat_byte(9)), (9, B256::ZERO), (u64::MAX, hash)] {
        assert!(matches!(
            h.cmd(Command::StakingProgress { block, block_hash }).as_slice(),
            [Event::AdminRejected { .. }]
        ));
        assert_eq!(h.engine.staking().unwrap().completed_through, Some(anchor));
    }
    assert!(h.cmd(Command::StakingProgress { block: 10, block_hash: B256::repeat_byte(10) }).is_empty());
    assert_eq!(h.engine.staking().unwrap().completed_through.unwrap().block, 10);
    let mut legacy = harness(ProtocolVersion::V25);
    assert!(matches!(
        legacy.cmd(Command::StakingProgress { block: 8, block_hash: hash }).as_slice(),
        [Event::AdminRejected { .. }]
    ));
    assert_eq!(legacy.engine.staking().unwrap().completed_through, None);
}
