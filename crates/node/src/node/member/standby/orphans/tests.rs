//! Which copies hold a tail no log will extend: an orphaned promotion's, or a superseded epoch's.

use std::collections::BTreeMap;

use alloy::primitives::Bytes;
use alloy::sol_types::SolCall;
use anyhow::bail;
use engine_core::command::Command;

use super::*;
use crate::liveness::chain::{headsCall, headsReturn};
use crate::log::{CheckpointDomain, MANIFEST};
use crate::standby::Follower;
use crate::standby_key::chain::{signerAtEpochCall, signerEpochCall};
use crate::tee::EnclaveKey;
use crate::test_support::{SEGMENT_COMMANDS, TestSequencer, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
const VAULT: Address = Address::repeat_byte(0x7a);
const LOG_HEADS: Address = Address::repeat_byte(0x4d);
/// The promoted member's standby key, which the returning operator follows.
const PROMOTED: AccountId = AccountId([0xb2; 20]);

/// A copy's tail is an orphaned promotion's only when another key than the followed enclave sealed
/// it, a final block well past its last stamp exists, and that block's recent signer epochs name the
/// key in none: a promotion still short of finality, or a key the vault named, keeps its tail.
#[test]
fn a_tail_is_orphaned_only_once_a_final_block_well_past_it_names_its_key_in_no_epoch() {
    let (incumbent, promoted, other) = (AccountId([0xa1; 20]), AccountId([0xb2; 20]), AccountId([0xc3; 20]));
    let stamp_ms: Millis = 1_800_000_000_000;
    let past = 1_800_000_000 + ORPHAN_MARGIN_S;
    let cases = [
        ("a removed promotion's key", promoted, past, vec![incumbent, other], true),
        ("the followed enclave's own tail", incumbent, past, vec![other], false),
        ("a promotion the final block names", promoted, past, vec![incumbent, promoted], false),
        ("a final block within the margin", promoted, past - 1, vec![incumbent], false),
    ];
    for (case, sealer, final_time_s, named, expected) in cases {
        assert_eq!(orphaned(sealer, incumbent, stamp_ms, final_time_s, &named), expected, "{case}");
    }
}

/// A tail is superseded only when the newest epoch naming its key has ended at the final block and the
/// copy runs past that epoch's last head on the same history.
#[test]
fn a_tail_is_superseded_only_past_the_last_head_of_its_keys_ended_epoch() {
    let (outgoing, promoted) = (AccountId([0xa1; 20]), AccountId([0xb2; 20]));
    let epochs = [(1, AccountId([0xc3; 20])), (2, outgoing), (3, promoted)];
    assert_eq!(ended_epoch(outgoing, 3, &epochs), Some(2), "a promotion ended the key's epoch");
    assert_eq!(ended_epoch(promoted, 3, &epochs), None, "the vault's latest epoch has not ended");
    assert_eq!(ended_epoch(AccountId([0xd4; 20]), 3, &epochs), None, "a key no recent epoch names");
    let reinstalled = [(2, outgoing), (3, promoted), (4, outgoing)];
    assert_eq!(ended_epoch(outgoing, 4, &reinstalled), None, "a key the latest epoch names again");
    let retired = [(2, outgoing)];
    assert_eq!(ended_epoch(outgoing, 2, &retired), None, "a retired signer's epoch is still the latest");

    let (link, other) = (B256::repeat_byte(0x11), B256::repeat_byte(0x22));
    let cases = [
        ("commands past the head on its history", (40, link), Some(link), 45, true),
        ("a copy ending at the head", (40, link), Some(link), 40, false),
        ("a copy short of the head", (40, link), None, 39, false),
        ("another history at the head", (40, link), Some(other), 45, false),
        ("an epoch that posted no head", (0, B256::ZERO), None, 45, false),
    ];
    for (case, head, held, last_seq, expected) in cases {
        assert_eq!(superseded(head, held, last_seq), expected, "{case}");
    }
}

/// A failed operator whose last commands reached no member, restarted as a member under the member
/// promoted in its place, finds its copy's tail superseded once a final block shows its epoch ended
/// with an earlier head, and sets the copy aside; before that, or with the copy at that head, or on
/// another history there, it keeps it.
#[tokio::test]
async fn a_returning_operators_tail_past_its_epochs_last_head_is_set_aside() {
    let dir = scratch_dir("orphans-superseded");
    let node = TestSequencer::start(&dir, 1024);
    for _ in 0..12 {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
    let outgoing = AccountId(EnclaveKey::test_key().address());
    let follower = Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, outgoing, [outgoing].into(), None, |_| {}).unwrap();
    let replica = follower.replica();
    let last = replica.last_seq().unwrap();
    let posted = replica.link_at(last - 3).unwrap();
    let stamp_s = u64::try_from(replica.link_at(last).unwrap().ts / 1000).unwrap();
    let promoted_final = FinalBlock {
        time: stamp_s + 60,
        latest: 2,
        signers: [(1, outgoing), (2, PROMOTED)].into(),
        heads: [(1, (posted.seq, posted.link))].into(),
    };
    let tail = |block: FinalBlock| async move {
        let seen = read_final(&block, (VAULT, LOG_HEADS), outgoing).await.unwrap();
        tail_of(&seen, replica, PROMOTED)
    };
    assert_eq!(
        tail(promoted_final.clone()).await,
        Some(Tail::Superseded { epoch: 1, head_seq: posted.seq }),
        "the commands past the epoch's last head",
    );
    let unfinal = FinalBlock { latest: 1, signers: [(1, outgoing)].into(), ..promoted_final.clone() };
    assert_eq!(tail(unfinal).await, None, "a promotion not final yet");
    let at_tip = replica.link_at(last).unwrap();
    let held = FinalBlock { heads: [(1, (at_tip.seq, at_tip.link))].into(), ..promoted_final.clone() };
    assert_eq!(tail(held).await, None, "every command was in a posted head");
    let forked = FinalBlock { heads: [(1, (posted.seq, B256::repeat_byte(0xee)))].into(), ..promoted_final.clone() };
    assert_eq!(tail(forked).await, None, "a copy on another history than the epoch's head");
    let unposted = FinalBlock { heads: BTreeMap::new(), ..promoted_final.clone() };
    assert_eq!(tail(unposted).await, None, "an epoch that posted no head");
    let seen = read_final(&promoted_final, (VAULT, LOG_HEADS), outgoing).await.unwrap();
    assert_eq!(tail_of(&seen, replica, outgoing), None, "the followed enclave's own tail");
    let removed = FinalBlock {
        time: stamp_s + ORPHAN_MARGIN_S,
        signers: [(1, AccountId([0xc3; 20])), (2, PROMOTED)].into(),
        ..promoted_final.clone()
    };
    assert_eq!(tail(removed).await, Some(Tail::Orphaned), "a key no epoch names is an orphaned promotion's");

    let aside = set_aside(&dir, &format!("superseded-{last}")).unwrap();
    assert!(!dir.exists(), "the copy is gone, so the member rebuilds it from the archive");
    assert!(Path::new(&aside).join(MANIFEST).exists(), "the set-aside copy keeps the tail");
    assert!(aside.to_string_lossy().ends_with(&format!(".superseded-{last}")));
}

// ── Helpers ──

/// A final block answering the vault's signer epochs and `LogHeads`' heads.
#[derive(Clone)]
struct FinalBlock {
    time: u64,
    latest: u64,
    signers: BTreeMap<u64, AccountId>,
    heads: BTreeMap<u64, (u64, B256)>,
}

impl AgreedBlock for FinalBlock {
    fn time(&self) -> u64 {
        self.time
    }

    fn number(&self) -> u64 {
        100
    }

    fn hash(&self) -> B256 {
        B256::repeat_byte(0x99)
    }

    async fn call(&self, _operation: &'static str, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        let out = if to == VAULT && data.starts_with(&signerEpochCall::SELECTOR) {
            signerEpochCall::abi_encode_returns(&self.latest)
        } else if to == VAULT && data.starts_with(&signerAtEpochCall::SELECTOR) {
            let epoch = signerAtEpochCall::abi_decode(&data)?.epoch;
            let signer = self.signers.get(&epoch).map_or(Address::ZERO, |signer| Address::from(signer.0));
            signerAtEpochCall::abi_encode_returns(&signer)
        } else if to == LOG_HEADS && data.starts_with(&headsCall::SELECTOR) {
            let epoch = headsCall::abi_decode(&data)?.epoch;
            let (seq, link) = self.heads.get(&epoch).copied().unwrap_or_default();
            headsCall::abi_encode_returns(&headsReturn { seq, link, signedAt: 0, boundBlock: 0, postedAt: 0 })
        } else {
            bail!("the final block answers no other call");
        };
        Ok(Bytes::from(out))
    }
}
