//! The operator's wiring of the liveness rule: what a confirmed handover leaves sealed, and which
//! recovered checkpoints a promoted start retires.

use super::*;
use crate::tee::keys::KeySet;
use crate::tee::sealing::MockSealer;
use crate::test_support::{dev_config, scratch_dir};

#[test]
fn a_confirmed_handover_deletes_the_sealed_copy_it_gave_away_and_keeps_other_generations() {
    let dir = scratch_dir("liveness-forget");
    let mut cfg = dev_config();
    cfg.tee.sealing = Some(toml::from_str(&format!("dir = {:?}", dir.display().to_string())).unwrap());
    let sealer = MockSealer { launch_digest: [0x11; 48] };
    let given = KeyStore::create(&dir, &sealer, cfg.file_sha256, 4, KeySet::generate().unwrap()).unwrap();
    let kept = KeyStore::create(&dir, &sealer, cfg.file_sha256, 5, KeySet::generate().unwrap()).unwrap();
    forget(&cfg, Some(&given));
    let file = |store: &KeyStore| crate::tee::sealing::file_for(&dir, &store.release(), store.generation());
    assert!(!file(&given).exists(), "the generation handed over no longer opens on this host");
    assert!(file(&kept).exists(), "another generation's keys stay");
    assert!(KeyStore::open(&dir, &sealer, cfg.file_sha256, 4).unwrap().is_none());
    forget(&cfg, Some(&given));
}

#[test]
fn a_promoted_start_retires_the_outgoing_keys_unaccepted_checkpoints() {
    let key = crate::tee::EnclaveKey::test_key();
    let work = |signer: String| crate::checkpoints::Work {
        checkpoint: crate::log::Checkpoint {
            parent: None,
            seq: 9,
            root: format!("0x{}", "ab".repeat(32)),
            signature: String::new(),
            signer,
            domain: None,
            deferred_withdrawals: true,
        },
        liabilities: engine_core::Fixed::ZERO,
        deposits: 0,
        authorized: engine_core::Fixed::ZERO,
        coverage: crate::checkpoints::DepositCoverage::Historical,
        distributed: engine_core::UsdcUnits(0),
    };
    let ours = work(key.address_hex());
    let outgoing = work(format!("{:#x}", Address::repeat_byte(0x99)));
    let promotion = crate::standby_key::chain::Promotion {
        turn: 1,
        position: 1,
        signer_epoch: 2,
        generation: 3,
        head_seq: 0,
        head_link: B256::ZERO,
        checkpoint_seq: 0,
        checkpoint_root: B256::ZERO,
    };
    let kept = retire(vec![ours.clone(), outgoing.clone()], Some(promotion), &key);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].checkpoint.signer, ours.checkpoint.signer);
    assert_eq!(retire(vec![ours, outgoing], None, &key).len(), 2, "any other start posts them all");
}
