//! What a promoted member's copy must hold before it starts.

use alloy::primitives::B256;
use engine_core::command::Command;

use super::*;
use crate::log::CheckpointDomain;
use crate::standby::Follower;
use crate::tee::EnclaveKey;
use crate::test_support::{CHECKPOINT_EVERY, SEGMENT_COMMANDS, TestSequencer, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

#[tokio::test]
async fn a_copy_lacking_either_prefix_the_promotion_fixed_names_what_it_lacks() {
    let dir = scratch_dir("promoted-prefixes");
    let node = TestSequencer::start(&dir, 1024);
    for _ in 0..12 {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
    let signer = AccountId(EnclaveKey::test_key().address());
    let follower = Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer, [signer].into(), None, |_| {}).unwrap();
    let replica = follower.replica();
    let last = replica.last_seq().unwrap();
    let checkpoint = replica.last_checkpoint().cloned().expect("the log holds a checkpoint");
    let head = replica.link_at(last - 1).unwrap();
    let promotion = Promotion {
        turn: 20_400,
        position: 1,
        signer_epoch: 2,
        generation: 9,
        head_seq: head.seq,
        head_link: head.link,
        checkpoint_seq: checkpoint.seq,
        checkpoint_root: checkpoint.root.parse().unwrap(),
        at: 0,
        block: 0,
    };
    assert_eq!(missing(replica, &promotion), None, "both prefixes held");
    let ahead = Promotion { head_seq: last + 3, ..promotion };
    let lacking = missing(replica, &ahead).unwrap();
    assert!(lacking.contains(&format!("seqs {}..={}", last + 1, last + 3)), "{lacking}");
    let forked = Promotion { head_link: B256::repeat_byte(0xee), ..promotion };
    assert!(missing(replica, &forked).unwrap().contains("holds link"), "another history at the head");
    let unaccepted = Promotion { checkpoint_root: B256::repeat_byte(0xee), ..promotion };
    assert!(missing(replica, &unaccepted).unwrap().contains("holds root"));
    let later = Promotion { checkpoint_seq: last + CHECKPOINT_EVERY, ..promotion };
    assert!(missing(replica, &later).unwrap().contains("up to the vault's latest checkpoint"));
    let none = Promotion { head_seq: 0, checkpoint_seq: 0, ..promotion };
    assert_eq!(missing(replica, &none), None, "a first epoch with no head and no checkpoint fixes nothing");
}

/// Setting a removed promotion's history aside records its phase first and moves the record last, so
/// a start after a crash at any step finishes it: here the key was already moved when the process
/// stopped, and the next quarantine moves the log and the record and leaves nothing to reopen.
#[test]
fn an_interrupted_quarantine_finishes_from_its_recorded_phase() {
    let dir = scratch_dir("promoted-quarantine");
    let (sealing, log) = (dir.join("sealing"), dir.join("log"));
    std::fs::create_dir_all(&sealing).unwrap();
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(log.join("segment"), b"history").unwrap();
    let key = sealing.join("key-9.sealed.json");
    std::fs::write(&key, b"sealed").unwrap();
    let record = Record::new(AccountId([0xca; 20]), AccountId([0x5b; 20]), PROMOTION, 121);
    record.write(&sealing).unwrap();
    // The first attempt stops after its phase and the key's move.
    Record { quarantining: true, ..record }.write(&sealing).unwrap();
    std::fs::rename(&key, sealing.join("key-9.sealed.json.quarantined-20400-1-0")).unwrap();
    let loaded = Record::load(&sealing).unwrap().expect("the record stays until the quarantine ends");
    assert!(loaded.quarantining, "a start finds the phase");
    quarantine(&log, &sealing, &key, &loaded).unwrap();
    assert!(!log.exists() && dir.join("log.quarantined-20400-1-0").join("segment").exists(), "the log is aside");
    assert!(Record::load(&sealing).unwrap().is_none(), "the record goes last");
    assert!(sealing.join(format!("{RECORD}.quarantined-20400-1-0")).exists());
    // A quarantine stopped by a failed rename has recorded its phase already: here the log's target is
    // taken, so the key moves and the log does not.
    let stopped = Record { turn: 20_402, ..record };
    std::fs::create_dir_all(&log).unwrap();
    std::fs::create_dir_all(dir.join("log.quarantined-20402-1-0").join("taken")).unwrap();
    std::fs::write(&key, b"sealed").unwrap();
    stopped.write(&sealing).unwrap();
    assert!(quarantine(&log, &sealing, &key, &stopped).is_err());
    assert!(!key.exists() && log.exists());
    assert!(Record::load(&sealing).unwrap().is_some_and(|loaded| loaded.quarantining), "the phase came first");
    std::fs::remove_dir_all(&log).unwrap();
    // A fresh quarantine, of another turn's promotion, moves everything.
    let fresh = Record { turn: 20_401, ..record };
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(&key, b"sealed").unwrap();
    fresh.write(&sealing).unwrap();
    quarantine(&log, &sealing, &key, &fresh).unwrap();
    assert!(!key.exists() && !log.exists() && Record::load(&sealing).unwrap().is_none());
}

/// The promotion the quarantine test records.
const PROMOTION: Promotion = Promotion {
    turn: 20_400,
    position: 1,
    signer_epoch: 2,
    generation: 9,
    head_seq: 120,
    head_link: B256::repeat_byte(0x4c),
    checkpoint_seq: 100,
    checkpoint_root: B256::repeat_byte(0x7e),
    at: 0,
    block: 0,
};
