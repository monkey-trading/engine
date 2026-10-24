//! Custody-backed audit regressions.

use std::fs;

use alloy::primitives::B256;
use engine_core::Fixed;
use engine_core::command::Command;
use tokio::sync::watch;

use super::*;
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
async fn missing_or_forged_accepted_history_does_not_authorize_a_log_named_old_signer() {
    let fixture = Fixture::new("audit-history-forgery");
    let original = fixture.chain.accepted.lock().unwrap().clone();
    fixture.chain.accepted.lock().unwrap().remove(&4);
    assert!(
        replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("missing accepted")
    );
    for field in 0..4 {
        *fixture.chain.accepted.lock().unwrap() = original.clone();
        {
            let mut accepted = fixture.chain.accepted.lock().unwrap();
            let old = accepted.get_mut(&4).unwrap();
            match field {
                0 => old.root = format!("0x{}", "ff".repeat(32)),
                1 => old.signer = fixture.signer(),
                2 => old.seq += 1,
                _ => old.signer_epoch = 3,
            }
        }
        assert!(
            replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.is_err(),
            "field {field}"
        );
    }
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
    assert!(
        build_trust(&fixture.path, &fixture.chain, fixture.domain, fixture.signer())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("differs")
    );
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
        fixture.chain.accepted.lock().unwrap().values().cloned().collect(),
    )
    .unwrap()
    .engine;
    append_command(&mut fixture.entries, &mut engine, Command::Tick);
    let tail = checkpoint(&mut engine, &fixture.current, fixture.domain, fixture.parent());
    fixture.entries.push(LogEntry::Checkpoint(tail));
    write_file(&fixture.path, &fixture.entries);
    let replayed = replay_with_chain(&fixture.path, &fixture.chain, fixture.domain, fixture.signer()).await.unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (7, 3));
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
async fn finalized_history_pages_reuse_one_pinned_canonical_observation() {
    let mut fixture = Fixture::new("audit-paged-history");
    let mut engine = crate::sequencer::replay_path_with_authorities(
        &fixture.path,
        fixture.domain,
        fixture.signer(),
        fixture.parent(),
        fixture.chain.accepted.lock().unwrap().values().cloned().collect(),
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
    assert_eq!(trust.authorities.len(), APPROVAL_PAGE_SIZE + 2);
    let pages = fixture.chain.pages.lock().unwrap();
    assert_eq!(pages.iter().map(|(_, page)| page.len()).collect::<Vec<_>>(), [APPROVAL_PAGE_SIZE, 2]);
    assert!(pages.iter().all(|(hash, _)| *hash == B256::repeat_byte(0x88)));
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
