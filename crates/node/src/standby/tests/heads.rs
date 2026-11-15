//! A follower takes the signed heads the operator's stream carries, as the API writes them.

use alloy::primitives::{Address, B256};

use super::*;
use crate::log_heads::{Head, HeadV2, Hold, SignedHead};
use crate::standby::follow::signed_head;

/// A `log_head` message as the signed stream writes it, tagged with its type beside the signed head's
/// fields, is taken: its head is recorded for the member link to relay and check.
#[test]
fn a_tagged_log_head_message_from_the_followed_enclave_is_taken() {
    let cfg = config();
    let vault = Address::from(cfg.custody.vault.parse::<AccountId>().unwrap().0);
    let head = HeadV2 {
        head: Head { seq: 7, link: B256::repeat_byte(7) },
        parent: None,
        turn: 20_400,
        position: 1,
        release: B256::repeat_byte(0x5e),
        generation: 3,
        stamp: 1_000,
        signed_at: 2_000,
        hold: Hold::Live,
        confirmer: Address::repeat_byte(0xc1),
        block_number: 0,
        block_hash: B256::ZERO,
    };
    let signature = EnclaveKey::test_key().sign_digest(head.digest(cfg.custody.chain_id, vault, 2));
    let signed = SignedHead { epoch: 2, head, signature };
    let mut message = serde_json::to_value(&signed).unwrap();
    message.as_object_mut().unwrap().insert("type".into(), "log_head".into());
    let link = crate::liveness::link::Copy::default();
    signed_head(&link, message, &cfg, &Followed::from_standby(&standby_section(1))).unwrap();
    assert_eq!(link.signed(), Some(signed), "the stream's head is kept for relaying");
}
