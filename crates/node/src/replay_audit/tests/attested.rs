//! Replays against independently attested signers, one per signer epoch.

use super::*;

#[test]
fn attested_replay_verifies_each_signer_epoch_and_refuses_the_unattested() {
    use std::collections::BTreeSet;

    use crate::tee::EnclaveKey;

    let fixture = Fixture::new("attested-multi-signer");
    let old = AccountId(fixture.old.address());
    let both: BTreeSet<_> = [old, fixture.signer()].into_iter().collect();
    let replayed =
        crate::replay_path_with_attested(&fixture.path, Some(fixture.domain), both.clone(), Default::default())
            .unwrap();
    assert_eq!(replayed.checkpoint_signers, both, "both signer epochs verify, one attestation each");
    for signers in [
        [fixture.signer()].into_iter().collect(),
        [old].into_iter().collect(),
        [AccountId(EnclaveKey::generate().address())].into_iter().collect(),
        Default::default(),
    ] {
        assert!(
            crate::replay_path_with_attested(&fixture.path, Some(fixture.domain), signers, Default::default()).is_err(),
            "the unattested set must not verify"
        );
    }
}

#[test]
fn attested_replay_refuses_a_tail_no_attested_signer_authenticated() {
    use std::collections::BTreeSet;

    use engine_core::command::{Command, Sequenced};

    let fixture = Fixture::new("attested-tail");
    let dir = fixture.path.parent().unwrap().join("attested-tail-forged.jsonl");
    // A host-appended command after the last checkpoint, with no seal: single-signer replay
    // never accepted this; the attested path must not either.
    let mut forged = fs::read_to_string(&fixture.path).unwrap();
    let tail = crate::log::LogEntry::Command(crate::log::CommandEntry {
        sequenced: Sequenced { seq: 99, ts: 1_000, cmd: Command::Tick },
        events: Some(Vec::new()),
        seal: None,
    });
    forged.push_str(&serde_json::to_string(&tail).unwrap());
    forged.push('\n');
    fs::write(&dir, forged).unwrap();
    let both: BTreeSet<_> = [AccountId(fixture.old.address()), fixture.signer()].into_iter().collect();
    assert!(
        crate::replay_path_with_attested(&dir, Some(fixture.domain), both.clone(), Default::default()).is_err(),
        "an unsealed tail passed the attested audit"
    );
    // Sanity: the untouched fixture still passes.
    let replayed =
        crate::replay_path_with_attested(&fixture.path, Some(fixture.domain), both, Default::default()).unwrap();
    assert_eq!(replayed.checkpoint_signers.len(), 2);
}
