//! Custody-backed audit regressions.

use std::fs;

use alloy::primitives::B256;
use engine_core::Fixed;
use engine_core::command::Command;
use tokio::sync::watch;

use super::*;
use crate::checkpoints::APPROVAL_PAGE_SIZE;
use crate::log::Manifest;
use crate::test_support::{TRADER, dev_config, scratch_dir};

mod fixture;
use fixture::{Fixture, accepted, append_command, checkpoint, checkpoint_parent, write_file};

#[tokio::test]
async fn custody_audit_verifies_old_and_current_signatures_without_modifying_the_public_log() {
    let fixture = Fixture::new("audit-rotation");
    let before = fs::read(&fixture.path).unwrap();
    let replayed = replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (6, 2));
    assert_eq!(replayed.engine.accounts()[&TRADER].balance, Fixed::from_int(60));
    assert_eq!(replayed.engine.withdrawal_approvals().count(), 1);
    assert_eq!(replayed.checkpoint_signers.len(), 2);
    assert!(replayed.checkpoint_signers.contains(&AccountId(fixture.old.address())));
    assert!(replayed.checkpoint_signers.contains(&fixture.signer()));
    assert_eq!(fs::read(&fixture.path).unwrap(), before);
    assert_eq!(fs::read_dir(fixture.path.parent().unwrap()).unwrap().count(), 1);
    assert!(
        crate::sequencer::replay_path_with_trust(&fixture.path, Some(fixture.domain), Some(fixture.signer())).is_err(),
        "the existing offline single-signer mode remains strict"
    );
}

#[tokio::test]
async fn missing_or_forged_anchor_or_epoch_history_does_not_authorize_a_log_named_old_signer() {
    let fixture = Fixture::new("audit-history-forgery");
    let original = fixture.chain.accepted.lock().unwrap().clone();
    let anchor = fixture.parent().seq;
    fixture.chain.accepted.lock().unwrap().remove(&anchor);
    assert!(
        replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("missing accepted")
    );
    for field in 0..5 {
        *fixture.chain.accepted.lock().unwrap() = original.clone();
        {
            let mut accepted = fixture.chain.accepted.lock().unwrap();
            let forged = accepted.get_mut(&anchor).unwrap();
            match field {
                0 => forged.root = format!("0x{}", "ff".repeat(32)),
                1 => forged.signer = AccountId(fixture.old.address()),
                2 => forged.seq += 1,
                3 => forged.signer_epoch = 3,
                // The current signer named under the retired signer's epoch.
                _ => forged.signer_epoch = 1,
            }
        }
        assert!(
            replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.is_err(),
            "field {field}"
        );
    }
    *fixture.chain.accepted.lock().unwrap() = original;
    fixture.chain.epochs.lock().unwrap().remove(&1);
    assert!(
        replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.is_err(),
        "a retired signer needs its epoch in the vault's history"
    );
}

#[tokio::test]
async fn a_short_log_or_wrong_anchor_fails_before_historical_metadata_is_requested() {
    let mut fixture = Fixture::new("audit-short-log");
    fixture.entries.retain(|entry| !matches!(entry, LogEntry::Checkpoint(cp) if cp.seq == 5));
    write_file(&fixture.path, &fixture.entries);
    assert!(
        build_trust(&fixture.path, &fixture.chain, fixture.domain, fixture.signer())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("anchor")
    );
    assert!(fixture.chain.pages.lock().unwrap().is_empty());
    let LogEntry::Checkpoint(old) =
        fixture.entries.iter().find(|entry| matches!(entry, LogEntry::Checkpoint(_))).unwrap()
    else {
        unreachable!()
    };
    let mut wrong = old.clone();
    wrong.seq = 5;
    wrong.root = format!("0x{}", "ff".repeat(32));
    fixture.entries.push(LogEntry::Checkpoint(wrong));
    write_file(&fixture.path, &fixture.entries);
    let error = build_trust(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.err().unwrap();
    assert!(format!("{error:#}").contains("differs"), "{error:#}");
    assert!(fixture.chain.pages.lock().unwrap().is_empty());
}

#[tokio::test]
async fn independently_supplied_current_signer_is_required_before_trusting_any_history() {
    let fixture = Fixture::new("audit-wrong-current");
    for signer in [AccountId(fixture.old.address()), AccountId([0; 20])] {
        assert!(replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, signer).await.is_err());
        assert!(fixture.chain.pages.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_retired_vault_signer_is_audited_against_its_epoch_signer() {
    let mut fixture = Fixture::new("audit-retired-signer");
    fixture.chain.at.state.signer = AccountId([0; 20]);
    fixture.chain.at.lifecycle.as_mut().unwrap().signer = AccountId([0; 20]);
    let replayed = replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (6, 2));
    let error = replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, AccountId(fixture.old.address()))
        .await
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("supplied trusted signer"), "{error:#}");
}

#[tokio::test]
async fn reissues_on_an_old_root_survive_two_rotations_without_an_intermediate_checkpoint() {
    let mut fixture = Fixture::new("audit-reissue-epochs");
    let middle = std::mem::replace(&mut fixture.current, crate::tee::EnclaveKey::generate());
    fixture.entries.retain(|entry| match entry {
        LogEntry::Command(command) => command.sequenced.seq <= 4,
        LogEntry::Checkpoint(cp) => cp.seq == 4,
        LogEntry::Withdrawal(payout) => payout.authorization.as_ref().unwrap().checkpoint_seq == 4,
    });
    fixture.chain.accepted.lock().unwrap().remove(&5);
    let accepted = fixture.chain.accepted.lock().unwrap().get(&4).unwrap().clone();
    fixture.chain.at.state.checkpoint = Some((4, accepted.root.clone()));
    fixture.chain.at.state.signer = fixture.signer();
    let signer = fixture.signer();
    let lifecycle = fixture.chain.at.lifecycle.as_mut().unwrap();
    lifecycle.signer = signer;
    lifecycle.signer_epoch = 3;
    lifecycle.checkpoint_seq = Some(4);
    lifecycle.checkpoint_root = Some(accepted.root);
    fixture.chain.epochs.lock().unwrap().insert(3, signer);
    let original = fixture
        .entries
        .iter()
        .find_map(|entry| match entry {
            LogEntry::Withdrawal(payout) => Some(payout.clone()),
            _ => None,
        })
        .unwrap();
    for (epoch, key) in [(2, &middle), (3, &fixture.current)] {
        let mut reissued = original.clone();
        let auth = reissued.authorization.as_mut().unwrap();
        auth.signer = AccountId(key.address());
        auth.signer_epoch = epoch;
        reissued.signature = key.sign_payout(&reissued).unwrap();
        fixture.entries.push(LogEntry::Withdrawal(reissued));
    }
    write_file(&fixture.path, &fixture.entries);
    let replayed = replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, signer).await.unwrap();
    assert_eq!((replayed.checkpoints, replayed.engine.withdrawal_approvals().count()), (1, 1));
    let pages = fixture.chain.epoch_pages.lock().unwrap().clone();
    assert_eq!(pages, [(B256::repeat_byte(0x88), vec![1, 2, 3])]);
    assert!(fixture.chain.accepted.lock().unwrap().values().all(|cp| cp.signer_epoch == 1));

    fixture.chain.epochs.lock().unwrap().insert(2, signer);
    assert!(
        replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, signer).await.is_err(),
        "a different epoch signer cannot authenticate the logged reissue"
    );
    fixture.chain.epochs.lock().unwrap().remove(&2);
    assert!(
        replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, signer).await.is_err(),
        "missing epoch history must never fall back to a log-named retired signer"
    );
}

#[tokio::test]
async fn tampered_withdrawal_proof_roots_fail_the_custody_backed_replay() {
    let mut fixture = Fixture::new("audit-proof-root");
    let withdrawal = fixture
        .entries
        .iter_mut()
        .find_map(|entry| match entry {
            LogEntry::Withdrawal(withdrawal) => Some(withdrawal),
            _ => None,
        })
        .unwrap();
    withdrawal.authorization.as_mut().unwrap().checkpoint_root = B256::repeat_byte(0xff);
    withdrawal.signature = fixture.old.sign_payout(withdrawal).unwrap();
    write_file(&fixture.path, &fixture.entries);
    assert!(
        replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.is_err(),
        "even a valid old signature cannot substitute its proof root"
    );
}

#[tokio::test]
async fn unknown_tail_still_requires_the_independently_trusted_current_signer() {
    let mut fixture = Fixture::new("audit-current-tail");
    let mut engine = crate::sequencer::replay_path_with_authorities(
        &fixture.path,
        fixture.domain,
        fixture.signer(),
        fixture.parent(),
        fixture.history(),
    )
    .unwrap()
    .engine;
    append_command(&mut fixture.entries, &mut engine, Command::Tick);
    let tail = checkpoint(&mut engine, &fixture.current, fixture.domain, fixture.parent());
    fixture.entries.push(LogEntry::Checkpoint(tail));
    write_file(&fixture.path, &fixture.entries);
    let replayed = replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (7, 3));
    let proof = engine.approval_proof(TRADER, 1).unwrap();
    let mut payout = fixture
        .entries
        .iter()
        .find_map(|entry| match entry {
            LogEntry::Withdrawal(payout) => Some(payout.clone()),
            _ => None,
        })
        .unwrap();
    let auth = payout.authorization.as_mut().unwrap();
    auth.signer = fixture.signer();
    auth.signer_epoch = 2;
    auth.checkpoint_seq = proof.seq;
    auth.checkpoint_root = proof.root;
    auth.siblings = proof.siblings;
    payout.signature = fixture.current.sign_payout(&payout).unwrap();
    fixture.entries.push(LogEntry::Withdrawal(payout));
    write_file(&fixture.path, &fixture.entries);
    let error = replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer())
        .await
        .err()
        .expect("a valid current signature cannot substitute an unaccepted local proof root");
    assert!(format!("{error:#}").contains("not accepted in the finalized prefix"), "{error:#}");
    assert!(matches!(fixture.entries.pop(), Some(LogEntry::Withdrawal(_))));
    let LogEntry::Checkpoint(tail) = fixture.entries.last_mut().unwrap() else { unreachable!() };
    tail.signer = fixture.old.address_hex();
    tail.signature = fixture.old.sign_checkpoint_v2(
        fixture.domain.chain_id,
        &fixture.domain.vault.0,
        tail.seq,
        &tail.root.parse::<engine_core::eip712::B256>().unwrap().0,
        tail.parent.unwrap(),
    );
    write_file(&fixture.path, &fixture.entries);
    assert!(replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.is_err());
}

#[tokio::test]
async fn a_long_accepted_history_costs_one_anchor_read_and_one_read_per_signer_epoch() {
    let mut fixture = Fixture::new("audit-long-history");
    let mut engine = crate::sequencer::replay_path_with_authorities(
        &fixture.path,
        fixture.domain,
        fixture.signer(),
        fixture.parent(),
        fixture.history(),
    )
    .unwrap()
    .engine;
    let mut parent = fixture.parent();
    for _ in 0..APPROVAL_PAGE_SIZE {
        append_command(&mut fixture.entries, &mut engine, Command::Tick);
        let cp = checkpoint(&mut engine, &fixture.current, fixture.domain, parent);
        parent = checkpoint_parent(&cp);
        fixture.chain.accepted.lock().unwrap().insert(cp.seq, accepted(&cp, 2));
        fixture.entries.push(LogEntry::Checkpoint(cp));
    }
    fixture.chain.at.state.checkpoint = Some((parent.seq, parent.root.to_string()));
    let lifecycle = fixture.chain.at.lifecycle.as_mut().unwrap();
    lifecycle.checkpoint_seq = Some(parent.seq);
    lifecycle.checkpoint_root = Some(parent.root.to_string());
    write_file(&fixture.path, &fixture.entries);
    let trust = build_trust(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.unwrap();
    let derived = &trust.authorities.checkpoints;
    assert_eq!(derived.len(), APPROVAL_PAGE_SIZE + 2);
    assert_eq!(derived[0], accepted(&fixture.first(), 1), "the retired signer keeps its own epoch");
    assert!(derived[1..].iter().all(|authority| authority.signer == fixture.signer() && authority.signer_epoch == 2));
    assert_eq!(*fixture.chain.pages.lock().unwrap(), [(B256::repeat_byte(0x88), vec![parent.seq])]);
    assert_eq!(*fixture.chain.epoch_pages.lock().unwrap(), [(B256::repeat_byte(0x88), vec![1, 2])]);
}

#[tokio::test]
async fn a_checkpoint_off_the_signed_parent_chain_never_gains_historical_trust() {
    let mut fixture = Fixture::new("audit-broken-chain");
    let first = fixture.first();
    let unlinked = CheckpointParent { has_checkpoint: true, seq: first.seq, root: B256::repeat_byte(7) };
    for entry in &mut fixture.entries {
        if let LogEntry::Checkpoint(cp) = entry
            && cp.seq != first.seq
        {
            cp.parent = Some(unlinked);
            cp.signature = fixture.current.sign_checkpoint_v2(
                fixture.domain.chain_id,
                &fixture.domain.vault.0,
                cp.seq,
                &cp.root.parse::<B256>().unwrap().0,
                unlinked,
            );
        }
    }
    write_file(&fixture.path, &fixture.entries);
    let error = build_trust(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.unwrap_err();
    assert!(error.to_string().contains("breaks the signed parent chain"), "{error:#}");
    assert!(fixture.chain.pages.lock().unwrap().is_empty(), "no vault read precedes the local chain check");
}

#[tokio::test]
async fn a_retired_checkpoint_signer_is_placed_in_its_epoch_without_a_payout_naming_it() {
    let mut fixture = Fixture::new("audit-retired-signer-epoch");
    fixture.entries.retain(|entry| !matches!(entry, LogEntry::Withdrawal(_)));
    write_file(&fixture.path, &fixture.entries);
    let trust = build_trust(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.unwrap();
    assert_eq!(trust.authorities.checkpoints[0], accepted(&fixture.first(), 1));
    assert_eq!(*fixture.chain.epoch_pages.lock().unwrap(), [(B256::repeat_byte(0x88), vec![1, 2])]);
}

#[tokio::test]
async fn a_checkpoint_signer_outside_the_vault_epoch_history_is_refused() {
    let fixture = Fixture::new("audit-unknown-signer");
    fixture.chain.epochs.lock().unwrap().insert(1, AccountId([5; 20]));
    let error = build_trust(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.unwrap_err();
    assert!(error.to_string().contains("absent from the vault's signer history"), "{error:#}");
}

#[tokio::test]
async fn segmented_log_audit_never_repairs_an_incomplete_manifest_tail() {
    let fixture = Fixture::new("audit-segments-source");
    let dir = scratch_dir("audit-segments-readonly");
    let segmented = SegmentedLog::open(&dir, |_| Ok(())).unwrap();
    let mut writer = segmented.into_writer(100, watch::channel(Manifest::default()).0).unwrap();
    for entry in &fixture.entries {
        writer.append(entry).unwrap();
    }
    writer.sync().unwrap();
    drop(writer);
    fs::write(dir.join("manifest.jsonl"), b"{partial").unwrap();
    let before = fs::read(dir.join("manifest.jsonl")).unwrap();
    let replayed = replay_with_chain(&dir, &fixture.chain, fixture.domain, fixture.signer()).await.unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (6, 2));
    assert_eq!(fs::read(dir.join("manifest.jsonl")).unwrap(), before);
    assert!(!dir.join("checkpoint-submission.json").exists());
}

#[tokio::test]
async fn mock_custody_config_cannot_enable_historical_signer_trust() {
    let fixture = Fixture::new("audit-mock-config");
    assert!(
        replay_path_with_custody(&fixture.path, &dev_config(), fixture.signer())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("EVM")
    );
}

#[tokio::test]
async fn replay_pass_rechecks_the_finalized_anchor_after_candidate_collection() {
    let mut fixture = Fixture::new("audit-anchor-between-passes");
    let trust = build_trust(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.unwrap();
    fixture.entries.retain(|entry| match entry {
        LogEntry::Checkpoint(cp) => cp.seq != trust.parent.seq,
        LogEntry::Withdrawal(withdrawal) => {
            withdrawal.authorization.as_ref().is_none_or(|auth| auth.checkpoint_seq != trust.parent.seq)
        }
        _ => true,
    });
    write_file(&fixture.path, &fixture.entries);
    assert!(
        crate::sequencer::replay_path_with_authorities(
            &fixture.path,
            fixture.domain,
            fixture.signer(),
            trust.parent,
            trust.authorities,
        )
        .is_err(),
        "the actual replay pass must contain the finalized checkpoint"
    );
}

#[tokio::test]
async fn absent_or_inconsistent_canonical_metadata_never_grants_historical_trust() {
    let mut fixture = Fixture::new("audit-invalid-observation");
    let canonical = fixture.chain.at.clone();
    for field in 0..4 {
        fixture.chain.at = canonical.clone();
        match field {
            0 => fixture.chain.at.view = View::Latest,
            1 => fixture.chain.at.lifecycle = None,
            2 => fixture.chain.at.lifecycle.as_mut().unwrap().signer_epoch = 0,
            _ => fixture.chain.at.lifecycle.as_mut().unwrap().height += 1,
        }
        assert!(
            build_trust(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.is_err(),
            "field {field}"
        );
        assert!(fixture.chain.pages.lock().unwrap().is_empty());
    }
}

#[test]
fn attested_replay_verifies_each_signer_epoch_and_refuses_the_unattested() {
    use std::collections::BTreeSet;

    use crate::tee::EnclaveKey;

    let fixture = Fixture::new("attested-multi-signer");
    let old = AccountId(fixture.old.address());
    let both: BTreeSet<_> = [old, fixture.signer()].into_iter().collect();
    let replayed = crate::replay_path_with_attested(&fixture.path, Some(fixture.domain), both.clone()).unwrap();
    assert_eq!(replayed.checkpoint_signers, both, "both signer epochs verify, one attestation each");
    for signers in [
        [fixture.signer()].into_iter().collect(),
        [old].into_iter().collect(),
        [AccountId(EnclaveKey::generate().address())].into_iter().collect(),
        Default::default(),
    ] {
        assert!(
            crate::replay_path_with_attested(&fixture.path, Some(fixture.domain), signers).is_err(),
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
        crate::replay_path_with_attested(&dir, Some(fixture.domain), both.clone()).is_err(),
        "an unsealed tail passed the attested audit"
    );
    // Sanity: the untouched fixture still passes.
    let replayed = crate::replay_path_with_attested(&fixture.path, Some(fixture.domain), both).unwrap();
    assert_eq!(replayed.checkpoint_signers.len(), 2);
}
