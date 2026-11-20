use super::*;
use crate::tee::EnclaveKey;
use engine_core::command::{Command, Sequenced};

fn command(seq: u64) -> CommandEntry {
    CommandEntry {
        sequenced: Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick },
        events: Some(Vec::new()),
        seal: None,
    }
}

#[test]
fn an_unsealed_span_cannot_be_laundered_by_a_later_sealed_command() {
    let key = EnclaveKey::test_key();
    let signer = AccountId(key.address());
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    let mut origin = Origin::new(Some(domain), Some(signer));
    origin.command(&command(0), ProtocolVersion::V2).unwrap();
    let mut later = command(1);
    later.seal = Some(key.seal_command(domain, command_link(origin.link, &later).unwrap()));
    origin.command(&later, ProtocolVersion::V2).unwrap();
    assert_eq!(
        origin.require_recovery(domain, signer).unwrap_err().to_string(),
        "unauthenticated command tail starts at seq 0"
    );
}

#[test]
fn current_key_sealed_tail_is_recoverable_without_its_next_checkpoint() {
    let key = EnclaveKey::test_key();
    let signer = AccountId(key.address());
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    let mut origin = Origin::new(Some(domain), Some(signer));
    let mut entry = command(0);
    let expected = command_link(origin.link, &entry).unwrap();
    entry.seal = Some(key.seal_command(domain, expected));
    origin.command(&entry, ProtocolVersion::V2).unwrap();
    origin.require_recovery(domain, signer).unwrap();
    assert_eq!(origin.link, expected);
    assert!(origin.require_recovery(domain, AccountId([1; 20])).is_err());
    let mut audit = Origin::default();
    audit.command(&entry, ProtocolVersion::V2).unwrap();
    assert!(audit.require_recovery(domain, signer).is_err());
}

#[test]
fn a_different_key_seal_cannot_authorize_an_unanchored_tail() {
    let key = EnclaveKey::test_key();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    let mut origin = Origin::new(Some(domain), Some(AccountId([1; 20])));
    let mut entry = command(0);
    entry.seal = Some(key.seal_command(domain, command_link(origin.link, &entry).unwrap()));
    origin.command(&entry, ProtocolVersion::V2).unwrap();
    assert_eq!(
        origin.require_recovery(domain, AccountId([1; 20])).unwrap_err().to_string(),
        "unauthenticated command tail starts at seq 0"
    );
}
#[test]
fn a_historical_staking_seal_needs_its_own_authorized_checkpoint_signer() {
    let historical = EnclaveKey::test_key();
    let current = EnclaveKey::generate();
    let current_signer = AccountId(current.address());
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    let mut origin = Origin::new(Some(domain), Some(current_signer));
    let mut entry = command(0);
    entry.sequenced.cmd = Command::Stake {
        account: AccountId([8; 20]),
        amount: engine_core::emissions::TokenUnits(1),
        block: 1,
        chain_ref: "old-stake".into(),
    };
    entry.seal = Some(historical.seal_command(domain, command_link(origin.link, &entry).unwrap()));
    origin.command(&entry, ProtocolVersion::V25).unwrap();
    let root = [1; 32];
    let checkpoint = Checkpoint {
        parent: None,
        seq: 0,
        root: format!("0x{}", hex::encode(root)),
        signature: historical.sign_checkpoint(domain.chain_id, &domain.vault.0, 0, &root),
        signer: historical.address_hex(),
        domain: Some(domain),
        deferred_withdrawals: false,
    };
    origin.checkpoint(&checkpoint, domain, true).unwrap();
    origin.require_recovery(domain, current_signer).unwrap();
}

/// A copy of another node's log refuses a command with no seal, or one sealed by a key it does not
/// accept, once a checkpoint covers it; a tail no checkpoint covers, an accepted signer's command
/// after it or not, is left to the caller, which `first_untrusted` names with its first sealer, and
/// which a handover's or a takeover's replay refuses and a follower's replica cuts back.
#[test]
fn a_copy_refuses_a_missing_or_foreign_seal_once_a_checkpoint_covers_it() {
    let (writer, foreign) = (EnclaveKey::test_key(), EnclaveKey::generate());
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    let accepted: BTreeSet<_> = [AccountId(writer.address())].into();
    let sealed = |origin: &Origin, seq: u64, key: Option<&EnclaveKey>| {
        let mut entry = command(seq);
        entry.seal = key.map(|key| key.seal_command(domain, command_link(origin.link, &entry).unwrap()));
        entry
    };
    let checkpoint = Checkpoint {
        parent: None,
        seq: 1,
        root: format!("0x{}", hex::encode([1; 32])),
        signature: String::new(),
        signer: writer.address_hex(),
        domain: Some(domain),
        deferred_withdrawals: false,
    };
    for (case, key) in [("no seal", None), ("a foreign seal", Some(&foreign))] {
        let mut origin = Origin::copy(domain, accepted.clone());
        origin.command(&sealed(&origin, 0, Some(&writer)), ProtocolVersion::LATEST).unwrap();
        let second = sealed(&origin, 1, key);
        origin.command(&second, ProtocolVersion::LATEST).unwrap_or_else(|e| panic!("{case}: refused uncovered: {e:#}"));
        let sealer = key.map(|key| AccountId(key.address()));
        assert_eq!(origin.first_untrusted(), Some((1, sealer)), "{case}: the tail's start or its sealer");
        // The uncovered tail opens a follower's replica, and a handover's or takeover's replay refuses it.
        assert!(origin.require_attested(&accepted).is_err(), "{case}: an uncovered foreign tail passed");
        let refused = origin.checkpoint(&checkpoint, domain, true).unwrap_err();
        assert!(refused.to_string().contains("seq 1 carries no seal by an accepted signer"), "{case}: {refused:#}");
    }
    // A foreign seal an accepted signer's command follows is still the copy's uncovered tail.
    let mut origin = Origin::copy(domain, accepted.clone());
    origin.command(&sealed(&origin, 0, Some(&foreign)), ProtocolVersion::LATEST).unwrap();
    origin.command(&sealed(&origin, 1, Some(&writer)), ProtocolVersion::LATEST).unwrap();
    assert_eq!(origin.first_untrusted(), Some((0, Some(AccountId(foreign.address())))), "the tail's start");
    assert!(origin.require_attested(&accepted).is_err(), "an uncovered foreign seal passed");
    // The writer's own seals pass, covered or not.
    let mut origin = Origin::copy(domain, accepted.clone());
    for seq in 0..2 {
        origin.command(&sealed(&origin, seq, Some(&writer)), ProtocolVersion::LATEST).unwrap();
    }
    origin.checkpoint(&checkpoint, domain, true).unwrap();
    origin.require_attested(&accepted).unwrap();
}
