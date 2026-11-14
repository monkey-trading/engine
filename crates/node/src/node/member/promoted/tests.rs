//! What a promoted member's copy must hold before it starts.

use alloy::primitives::B256;
use engine_core::command::Command;

use super::*;
use crate::log::CheckpointDomain;
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
