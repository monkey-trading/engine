//! The operator's wiring of the liveness rule: what a confirmed handover leaves sealed.

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
